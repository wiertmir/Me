mod support;

use axum::http::{HeaderMap, Method, StatusCode};
use icalendar::parser::{Component, read_calendar, unfold};
use roxmltree::Document;
use serde_json::{Value, json};
use support::{ALICE, CALDAV, DAV, Stack, named, responses, text};

const ITEM_PROPS: &str = r#"<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/">
  <D:prop><D:getetag/><D:getcontenttype/><D:resourcetype/><CS:getctag/></D:prop></D:propfind>"#;

const WARSAW_9_TO_10: &str = "DTSTART;TZID=Europe/Warsaw:20261005T090000\r\n\
                              DTEND;TZID=Europe/Warsaw:20261005T100000";

/// The path of alice's calendar, without a trailing slash.
async fn calendar(s: &Stack) -> String {
    let (_, calendars) = s
        .rest(Method::GET, "/calendar/v1/calendars", ALICE, None)
        .await;
    format!(
        "/dav/calendars/alice/c-{}",
        calendars[0]["id"].as_str().unwrap()
    )
}

fn vevent(uid: &str, lines: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20261005T080000Z\r\n{lines}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn put(s: &Stack, cal: &str, uid: &str, lines: &str, headers: &[(&str, &str)]) -> StatusCode {
    let path = format!("{cal}/{uid}.ics");
    let (status, h, _) = s
        .dav("PUT", &path, "alice", headers, &vevent(uid, lines))
        .await;
    // What is stored is not the body as sent, so the answer names no etag for it.
    assert!(!h.contains_key("etag"));
    status
}

async fn get(s: &Stack, cal: &str, uid: &str) -> (StatusCode, HeaderMap, String) {
    s.dav("GET", &format!("{cal}/{uid}.ics"), "alice", &[], "")
        .await
}

async fn etag(s: &Stack, cal: &str, uid: &str) -> String {
    let (st, h, _) = get(s, cal, uid).await;
    assert_eq!(st, StatusCode::OK);
    h["etag"].to_str().unwrap().to_owned()
}

/// The events calendar-service has under that uid: the single one or the series first.
async fn stored(s: &Stack, cal: &str, uid: &str) -> Vec<Value> {
    let id = cal.rsplit("/c-").next().unwrap();
    let (st, found) = s
        .rest(
            Method::GET,
            &format!("/calendar/v1/calendars/{id}/by-uid?uid={uid}"),
            ALICE,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    found.as_array().unwrap().clone()
}

/// The lines of one `VEVENT` of an answer as (name, parameters, value), and the `TRIGGER`s of its alarms.
struct Event {
    props: Vec<(String, String, String)>,
    alarms: Vec<String>,
}

impl Event {
    fn of(c: &Component) -> Self {
        assert_eq!(c.name, "VEVENT");
        let props = c.properties.iter().map(|p| {
            let params = p.params.iter().map(|q| {
                let val = q.val.as_ref().map(|v| v.to_string());
                format!("{}={}", q.key, val.unwrap_or_default())
            });
            let params: Vec<_> = params.collect();
            (p.name.to_string(), params.join(";"), p.val.to_string())
        });
        let alarms = c.components.iter().map(|a| {
            assert_eq!(a.name, "VALARM");
            assert_eq!(a.find_prop("ACTION").unwrap().val, "DISPLAY");
            a.find_prop("TRIGGER").unwrap().val.to_string()
        });
        Self {
            props: props.collect(),
            alarms: alarms.collect(),
        }
    }

    /// Every `VEVENT` of a body.
    fn all(body: &str) -> Vec<Self> {
        let unfolded = unfold(body);
        let cal = read_calendar(&unfolded).unwrap();
        cal.components.iter().map(Self::of).collect()
    }

    /// The parameters and value of each line of that name.
    fn lines(&self, name: &str) -> Vec<(&str, &str)> {
        let named = self.props.iter().filter(|p| p.0 == name);
        named.map(|p| (p.1.as_str(), p.2.as_str())).collect()
    }

    fn val(&self, name: &str) -> Option<&str> {
        let all = self.lines(name);
        assert!(all.len() <= 1, "{name} more than once");
        all.first().map(|l| l.1)
    }
}

/// The one `VEVENT` of the item.
async fn read(s: &Stack, cal: &str, uid: &str) -> Event {
    let (st, _, body) = get(s, cal, uid).await;
    assert_eq!(st, StatusCode::OK);
    let mut all = Event::all(&body);
    assert_eq!(all.len(), 1, "{body}");
    all.remove(0)
}

#[tokio::test]
async fn timed_event_round_trip() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = format!(
        "{WARSAW_9_TO_10}\r\nSUMMARY:Dentist\r\nLOCATION:Main St 1\\, Warsaw\r\nDESCRIPTION:bring x-rays"
    );
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::CREATED);

    let ev = &stored(&s, &c, "e1").await[0];
    assert_eq!(ev["uid"], "e1");
    assert_eq!(ev["start"], "2026-10-05T09:00:00");
    assert_eq!(ev["end"], "2026-10-05T10:00:00");
    assert_eq!(ev["tz"], "Europe/Warsaw");
    assert_eq!(ev["all_day"], false);
    assert_eq!(ev["summary"], "Dentist");
    assert_eq!(ev["location"], "Main St 1, Warsaw");
    assert_eq!(ev["description"], "bring x-rays");

    let (st, h, ics) = get(&s, &c, "e1").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h["content-type"], "text/calendar; charset=utf-8");
    assert!(h.contains_key("etag"));
    assert!(!ics.contains("VTIMEZONE"));
    let e = read(&s, &c, "e1").await;
    assert_eq!(e.val("UID"), Some("e1"));
    assert_eq!(
        e.lines("DTSTART"),
        [("TZID=Europe/Warsaw", "20261005T090000")]
    );
    assert_eq!(
        e.lines("DTEND"),
        [("TZID=Europe/Warsaw", "20261005T100000")]
    );
    assert_eq!(e.val("SUMMARY"), Some("Dentist"));
    assert_eq!(e.val("LOCATION"), Some("Main St 1, Warsaw"));
    assert_eq!(e.val("DESCRIPTION"), Some("bring x-rays"));
    assert!(e.val("DTSTAMP").unwrap().ends_with('Z'));
    assert!(e.val("LAST-MODIFIED").unwrap().ends_with('Z'));
    assert_eq!(e.val("RRULE"), None);
    assert!(e.alarms.is_empty());
}

#[tokio::test]
async fn utc_event_is_stored_as_utc() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = "DTSTART:20261005T070000Z\r\nDTEND:20261005T080000Z";
    assert_eq!(put(&s, &c, "e1", body, &[]).await, StatusCode::CREATED);
    let ev = &stored(&s, &c, "e1").await[0];
    assert_eq!(ev["tz"], "UTC");
    assert_eq!(ev["start"], "2026-10-05T07:00:00");
    assert_eq!(ev["end"], "2026-10-05T08:00:00");
    let e = read(&s, &c, "e1").await;
    assert_eq!(e.lines("DTSTART"), [("TZID=UTC", "20261005T070000")]);
}

#[tokio::test]
async fn end_in_another_zone_is_read_in_the_starts() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    // 08:00 UTC and 11:00 in Kyiv are both 10:00 in Warsaw that day.
    for (uid, end) in [
        ("e1", "DTEND:20261005T080000Z"),
        ("e2", "DTEND;TZID=Europe/Kyiv:20261005T110000"),
    ] {
        let body = format!("DTSTART;TZID=Europe/Warsaw:20261005T090000\r\n{end}");
        assert_eq!(put(&s, &c, uid, &body, &[]).await, StatusCode::CREATED);
        let ev = &stored(&s, &c, uid).await[0];
        assert_eq!(ev["tz"], "Europe/Warsaw");
        assert_eq!(ev["end"], "2026-10-05T10:00:00", "{uid}");
    }
}

#[tokio::test]
async fn all_day_event_round_trip() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = "DTSTART;VALUE=DATE:20261005\r\nDTEND;VALUE=DATE:20261007";
    assert_eq!(put(&s, &c, "e1", body, &[]).await, StatusCode::CREATED);
    let ev = &stored(&s, &c, "e1").await[0];
    assert_eq!(ev["all_day"], true);
    assert_eq!(ev["start"], "2026-10-05");
    assert_eq!(ev["end"], "2026-10-07");
    assert_eq!(ev["tz"], Value::Null);
    let e = read(&s, &c, "e1").await;
    assert_eq!(e.lines("DTSTART"), [("VALUE=DATE", "20261005")]);
    assert_eq!(e.lines("DTEND"), [("VALUE=DATE", "20261007")]);

    let body = "DTSTART;VALUE=DATE:20261005";
    assert_eq!(put(&s, &c, "e2", body, &[]).await, StatusCode::CREATED);
    let ev = &stored(&s, &c, "e2").await[0];
    assert_eq!(ev["all_day"], true);
    assert_eq!(ev["end"], "2026-10-06");

    let body = "DTSTART;VALUE=DATE:20261005\r\nDURATION:P3D";
    assert_eq!(put(&s, &c, "e3", body, &[]).await, StatusCode::CREATED);
    assert_eq!(stored(&s, &c, "e3").await[0]["end"], "2026-10-08");
}

#[tokio::test]
async fn duration_instead_of_dtend() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = "DTSTART;TZID=Europe/Warsaw:20261005T090000\r\nDURATION:PT90M";
    assert_eq!(put(&s, &c, "e1", body, &[]).await, StatusCode::CREATED);
    assert_eq!(stored(&s, &c, "e1").await[0]["end"], "2026-10-05T10:30:00");
    let e = read(&s, &c, "e1").await;
    assert_eq!(
        e.lines("DTEND"),
        [("TZID=Europe/Warsaw", "20261005T103000")]
    );
    assert_eq!(e.val("DURATION"), None);
}

#[tokio::test]
async fn reminders_round_trip() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let alarm = |trigger: &str| {
        format!("BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:x\r\n{trigger}\r\nEND:VALARM")
    };
    // An alarm at a fixed time, one after the start and one counted from the end are dropped.
    let body = [
        WARSAW_9_TO_10.to_owned(),
        alarm("TRIGGER:-PT10M"),
        alarm("TRIGGER;VALUE=DATE-TIME:20261005T060000Z"),
        alarm("TRIGGER:PT5M"),
        alarm("TRIGGER;RELATED=END:-PT15M"),
        alarm("TRIGGER:PT0S"),
    ]
    .join("\r\n");
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::CREATED);
    assert_eq!(stored(&s, &c, "e1").await[0]["reminders"], json!([10, 0]));
    let (_, _, ics) = get(&s, &c, "e1").await;
    assert!(!ics.contains("RELATED"), "{ics}");
    assert_eq!(read(&s, &c, "e1").await.alarms, ["-PT10M", "-PT0M"]);
}

#[tokio::test]
async fn change_and_delete() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = format!("{WARSAW_9_TO_10}\r\nSUMMARY:Dentist");
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::CREATED);
    let first = etag(&s, &c, "e1").await;

    let body = format!("{WARSAW_9_TO_10}\r\nSUMMARY:Dentist, again");
    let st = put(&s, &c, "e1", &body, &[("if-match", &first)]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let second = etag(&s, &c, "e1").await;
    assert_ne!(second, first);
    assert_eq!(
        read(&s, &c, "e1").await.val("SUMMARY"),
        Some("Dentist, again")
    );
    assert_eq!(stored(&s, &c, "e1").await.len(), 1);

    let path = format!("{c}/e1.ics");
    let (st, _, _) = s
        .dav("DELETE", &path, "alice", &[("if-match", &second)], "")
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(get(&s, &c, "e1").await.0, StatusCode::NOT_FOUND);
    assert!(stored(&s, &c, "e1").await.is_empty());
    let (st, _, _) = s.dav("DELETE", &path, "alice", &[], "").await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn series_without_exceptions() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = format!(
        "{WARSAW_9_TO_10}\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nEXDATE;TZID=Europe/Warsaw:20261012T090000"
    );
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::CREATED);
    let ev = &stored(&s, &c, "e1").await[0];
    assert_eq!(ev["rrule"], "FREQ=WEEKLY;COUNT=4");
    assert_eq!(ev["exdates"], json!(["2026-10-12T09:00:00"]));
    let e = read(&s, &c, "e1").await;
    assert_eq!(e.val("RRULE"), Some("FREQ=WEEKLY;COUNT=4"));
    assert_eq!(
        e.lines("EXDATE"),
        [("TZID=Europe/Warsaw", "20261012T090000")]
    );

    // Several values on a line, several lines, and a value in UTC: all in the form of DTSTART.
    let body = format!(
        "{WARSAW_9_TO_10}\r\nRRULE:FREQ=WEEKLY;COUNT=6\r\n\
         EXDATE;TZID=Europe/Warsaw:20261012T090000,20261019T090000\r\nEXDATE:20261102T080000Z"
    );
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::NO_CONTENT);
    let ev = &stored(&s, &c, "e1").await[0];
    assert_eq!(
        ev["exdates"],
        json!([
            "2026-10-12T09:00:00",
            "2026-10-19T09:00:00",
            "2026-11-02T09:00:00"
        ])
    );
    assert_eq!(read(&s, &c, "e1").await.lines("EXDATE").len(), 3);

    let body =
        "DTSTART;VALUE=DATE:20261005\r\nRRULE:FREQ=DAILY;COUNT=3\r\nEXDATE;VALUE=DATE:20261006";
    assert_eq!(put(&s, &c, "e2", body, &[]).await, StatusCode::CREATED);
    assert_eq!(
        stored(&s, &c, "e2").await[0]["exdates"],
        json!(["2026-10-06"])
    );
    let e = read(&s, &c, "e2").await;
    assert_eq!(e.lines("EXDATE"), [("VALUE=DATE", "20261006")]);
}

/// A changed occurrence made elsewhere is part of the item: it is sent, and it changes the etag.
#[tokio::test]
async fn override_made_elsewhere_is_part_of_the_item() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = format!("{WARSAW_9_TO_10}\r\nRRULE:FREQ=WEEKLY;COUNT=4");
    assert_eq!(put(&s, &c, "e1", &body, &[]).await, StatusCode::CREATED);
    let before = etag(&s, &c, "e1").await;
    let series = stored(&s, &c, "e1").await[0]["id"].clone();
    let id = c.rsplit("/c-").next().unwrap();
    let moved = json!({"summary": "moved", "all_day": false, "start": "2026-10-12T11:00:00",
        "end": "2026-10-12T12:00:00", "tz": "Europe/Warsaw", "recurring_event_id": series,
        "original_start": "2026-10-12T09:00:00"});
    let (st, _) = s
        .rest(
            Method::POST,
            &format!("/calendar/v1/calendars/{id}/events"),
            ALICE,
            Some(moved),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);

    let (_, h, ics) = get(&s, &c, "e1").await;
    assert_ne!(h["etag"], before.as_str());
    let all = Event::all(&ics);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].val("RRULE"), Some("FREQ=WEEKLY;COUNT=4"));
    assert_eq!(all[0].val("RECURRENCE-ID"), None);
    assert_eq!(
        all[1].lines("RECURRENCE-ID"),
        [("TZID=Europe/Warsaw", "20261012T090000")]
    );
    assert_eq!(all[1].val("UID"), Some("e1"));
    assert_eq!(all[1].val("SUMMARY"), Some("moved"));

    // One item in the listing, with the same etag.
    let (_, _, xml) = s
        .dav(
            "PROPFIND",
            &format!("{c}/"),
            "alice",
            &[("depth", "1")],
            ITEM_PROPS,
        )
        .await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 2);
    assert_eq!(text(r[1].1, DAV, "getetag"), h["etag"].to_str().unwrap());
}

#[tokio::test]
async fn listing_and_reports() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let mut etags = Vec::new();
    for uid in ["e1", "e2", "e3"] {
        assert_eq!(
            put(&s, &c, uid, WARSAW_9_TO_10, &[]).await,
            StatusCode::CREATED
        );
        etags.push(etag(&s, &c, uid).await);
    }
    let href = |uid: &str| format!("{c}/{uid}.ics");

    let collection = format!("{c}/");
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
    for (uid, etag) in ["e1", "e2", "e3"].iter().zip(&etags) {
        let (_, item) = r.iter().find(|(h, _)| *h == href(uid)).unwrap();
        assert_eq!(&text(*item, DAV, "getetag"), etag);
        assert!(text(*item, DAV, "getcontenttype").starts_with("text/calendar"));
    }
    let (st, _, xml) = s
        .dav("PROPFIND", &collection, "alice", &[], ITEM_PROPS)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    assert_eq!(responses(&Document::parse(&xml).unwrap()).len(), 1);

    let (st, _, xml) = s
        .dav("PROPFIND", &href("e2"), "alice", &[], ITEM_PROPS)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].0, href("e2"));
    assert_eq!(text(r[0].1, DAV, "getetag"), etags[1]);
    let (st, _, _) = s
        .dav("PROPFIND", &href("gone"), "alice", &[], ITEM_PROPS)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/><C:calendar-data/></D:prop>
        <D:href>{}</D:href><D:href>https://me.example:8443{}</D:href><D:href>{}</D:href></C:calendar-multiget>"#,
        href("e1"),
        href("e3"),
        href("gone")
    );
    let (st, _, xml) = s
        .dav("REPORT", &collection, "alice", &[("depth", "1")], &multiget)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 3);
    for (uid, etag) in [("e1", &etags[0]), ("e3", &etags[2])] {
        let (_, item) = r.iter().find(|(h, _)| *h == href(uid)).unwrap();
        assert_eq!(&text(*item, DAV, "getetag"), etag);
        let data = text(*item, CALDAV, "calendar-data");
        assert_eq!(Event::all(&data)[0].val("UID"), Some(uid));
    }
    let (_, gone) = r.iter().find(|(h, _)| *h == href("gone")).unwrap();
    assert!(text(*gone, DAV, "status").contains("404"));
    assert!(named(*gone, DAV, "propstat").is_none());

    let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/><C:calendar-data/></D:prop>
        <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT"/></C:comp-filter></C:filter>
        </C:calendar-query>"#;
    let (st, _, xml) = s
        .dav("REPORT", &collection, "alice", &[("depth", "1")], query)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 3);
    for (_, item) in &r {
        assert!(text(*item, CALDAV, "calendar-data").contains("BEGIN:VEVENT"));
    }

    let other =
        r#"<D:sync-collection xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:sync-collection>"#;
    let (st, _, _) = s.dav("REPORT", &collection, "alice", &[], other).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn preconditions() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let x = format!("{WARSAW_9_TO_10}\r\nSUMMARY:x");
    let y = format!("{WARSAW_9_TO_10}\r\nSUMMARY:y");
    let st = put(&s, &c, "e1", &x, &[("if-none-match", "*")]).await;
    assert_eq!(st, StatusCode::CREATED);
    let etag = etag(&s, &c, "e1").await;

    let st = put(&s, &c, "e1", &y, &[("if-none-match", "*")]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let st = put(&s, &c, "e1", &y, &[("if-match", "\"0-1\"")]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let path = format!("{c}/e1.ics");
    let (st, _, _) = s
        .dav("DELETE", &path, "alice", &[("if-match", "\"0-1\"")], "")
        .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    let st = put(&s, &c, "e2", &y, &[("if-match", &etag)]).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);

    assert_eq!(stored(&s, &c, "e1").await[0]["summary"], "x");
    assert!(stored(&s, &c, "e2").await.is_empty());

    let st = put(&s, &c, "e1", &y, &[("if-match", "*")]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &c, "e1").await[0]["summary"], "y");

    let (st, _, body) = s
        .dav("PUT", &path, "alice", &[], &vevent("other", &x))
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("valid-calendar-object-resource"), "{body}");
}

#[tokio::test]
async fn refusals() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let path = format!("{c}/e1.ics");
    let rule = "RRULE:FREQ=WEEKLY;COUNT=4";
    for lines in [
        "DTSTART:20261005T090000".to_owned(),
        "DTSTART;TZID=Mars/Olympus:20261005T090000".to_owned(),
        format!("{WARSAW_9_TO_10}\r\nRDATE;TZID=Europe/Warsaw:20261006T090000"),
        format!("{WARSAW_9_TO_10}\r\n{rule}\r\nRRULE:FREQ=DAILY"),
        format!("{WARSAW_9_TO_10}\r\n{rule}\r\nEXRULE:FREQ=MONTHLY"),
        format!("{WARSAW_9_TO_10}\r\n{rule}\r\nEXDATE:20261012T090000"),
        format!("{WARSAW_9_TO_10}\r\n{rule}\r\nEXDATE;VALUE=DATE:20261012"),
        "DTSTART;TZID=Europe/Warsaw:20261005T090000\r\nDTEND;VALUE=DATE:20261006".to_owned(),
        "DTSTART;TZID=Europe/Warsaw:20261005T090000\r\nDURATION:-PT1H".to_owned(),
        "SUMMARY:no start".to_owned(),
        // Changed occurrences come with the next task.
        format!("{WARSAW_9_TO_10}\r\nRECURRENCE-ID;TZID=Europe/Warsaw:20261005T090000"),
    ] {
        let (st, _, body) = s
            .dav("PUT", &path, "alice", &[], &vevent("e1", &lines))
            .await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{lines}");
        assert!(body.contains("valid-calendar-data"), "{lines}: {body}");
    }
    let two = vevent(
        "e1",
        &format!("{WARSAW_9_TO_10}\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:e1\r\n{WARSAW_9_TO_10}"),
    );
    let (st, _, body) = s.dav("PUT", &path, "alice", &[], &two).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("valid-calendar-data"), "{body}");

    let todo = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTODO\r\nUID:e1\r\nSUMMARY:x\r\n\
                END:VTODO\r\nEND:VCALENDAR\r\n";
    let (st, _, body) = s.dav("PUT", &path, "alice", &[], todo).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("supported-calendar-data"), "{body}");

    assert!(stored(&s, &c, "e1").await.is_empty());
}

#[tokio::test]
async fn service_validation_becomes_403() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let lines = "DTSTART;TZID=Europe/Warsaw:20261005T100000\r\n\
                 DTEND;TZID=Europe/Warsaw:20261005T090000";
    let (st, _, body) = s
        .dav(
            "PUT",
            &format!("{c}/e1.ics"),
            "alice",
            &[],
            &vevent("e1", lines),
        )
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(body.contains("valid-calendar-data"), "{body}");
    assert!(body.contains("end must be after start"), "{body}");
    assert!(stored(&s, &c, "e1").await.is_empty());
}

#[tokio::test]
async fn dropped_properties_stay_dropped() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let body = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VTIMEZONE\r\nTZID:Europe/Warsaw\r\n\
         BEGIN:STANDARD\r\nDTSTART:19701025T030000\r\nTZOFFSETFROM:+0200\r\nTZOFFSETTO:+0100\r\n\
         END:STANDARD\r\nEND:VTIMEZONE\r\nBEGIN:VEVENT\r\nUID:e1\r\n{WARSAW_9_TO_10}\r\nSUMMARY:x\r\n\
         ATTENDEE;CN=Bob:mailto:bob@example.com\r\nORGANIZER:mailto:alice@example.com\r\n\
         CATEGORIES:a,b\r\nSTATUS:TENTATIVE\r\nTRANSP:TRANSPARENT\r\nCLASS:PRIVATE\r\nSEQUENCE:3\r\n\
         X-FOO:bar\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    );
    let (st, _, _) = s
        .dav("PUT", &format!("{c}/e1.ics"), "alice", &[], &body)
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, _, ics) = get(&s, &c, "e1").await;
    assert!(!ics.contains("VTIMEZONE"), "{ics}");
    let e = read(&s, &c, "e1").await;
    for name in [
        "ATTENDEE",
        "ORGANIZER",
        "CATEGORIES",
        "STATUS",
        "TRANSP",
        "CLASS",
        "SEQUENCE",
        "X-FOO",
    ] {
        assert_eq!(e.val(name), None, "{name}");
    }
    assert_eq!(e.val("SUMMARY"), Some("x"));
}

#[tokio::test]
async fn change_made_elsewhere_shows() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    assert_eq!(
        put(&s, &c, "e1", WARSAW_9_TO_10, &[]).await,
        StatusCode::CREATED
    );
    let collection = format!("{c}/");
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
    let id = stored(&s, &c, "e1").await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let changed = json!({"summary": "changed elsewhere", "all_day": false, "start": "2026-10-05T09:00:00",
        "end": "2026-10-05T10:00:00", "tz": "Europe/Warsaw"});
    let (st, _) = s
        .rest(
            Method::PUT,
            &format!("/calendar/v1/events/{id}"),
            ALICE,
            Some(changed),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let after = marks().await;
    assert_ne!(after.0, before.0);
    assert_ne!(after.1, before.1);
    assert_eq!(
        read(&s, &c, "e1").await.val("SUMMARY"),
        Some("changed elsewhere")
    );
}

#[tokio::test]
async fn other_users_calendar_is_404() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, "e1", WARSAW_9_TO_10, &[]).await;
    // Bob's own name with alice's calendar: the service does not know it as his.
    let path = format!("{}/e1.ics", c.replace("/alice/", "/bob/"));
    for method in ["GET", "DELETE", "PROPFIND"] {
        let (st, _, _) = s.dav(method, &path, "bob", &[], "").await;
        assert_eq!(st, StatusCode::NOT_FOUND, "{method}");
    }
    let (st, _, _) = s
        .dav("PUT", &path, "bob", &[], &vevent("e1", WARSAW_9_TO_10))
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(stored(&s, &c, "e1").await.len(), 1);
}
