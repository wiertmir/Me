mod support;

use axum::http::{Method, StatusCode};
use roxmltree::Document;
use serde_json::{Value, json};
use support::{ALICE, CALDAV, DAV, Event, Stack, calendar, etag, get, responses, stored, text};

/// Mondays at 09:00 in Warsaw, from 5 October 2026.
const SERIES: &str = "DTSTART;TZID=Europe/Warsaw:20261005T090000\r\n\
                      DTEND;TZID=Europe/Warsaw:20261005T100000\r\n\
                      RRULE:FREQ=WEEKLY;COUNT=6\r\nSUMMARY:Weekly";

/// One `VCALENDAR` with a `VEVENT` per (uid, lines).
fn body(parts: &[(&str, &str)]) -> String {
    let vevent = |(uid, lines): &(&str, &str)| {
        format!(
            "BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20261005T080000Z\r\n{lines}\r\nEND:VEVENT\r\n"
        )
    };
    let events: String = parts.iter().map(vevent).collect();
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{events}END:VCALENDAR\r\n")
}

/// The item `s1` with those parts.
async fn put_as(s: &Stack, cal: &str, parts: &[&str]) -> (StatusCode, String) {
    let parts: Vec<_> = parts.iter().map(|lines| ("s1", *lines)).collect();
    let path = format!("{cal}/s1.ics");
    let (status, h, answer) = s.dav("PUT", &path, "alice", &[], &body(&parts)).await;
    assert!(!h.contains_key("etag"));
    (status, answer)
}

async fn put(s: &Stack, cal: &str, parts: &[&str]) -> StatusCode {
    put_as(s, cal, parts).await.0
}

async fn refused(s: &Stack, cal: &str, parts: &[&str]) {
    let (st, answer) = put_as(s, cal, parts).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{parts:?}");
    assert!(answer.contains("valid-calendar-data"), "{answer}");
}

/// The occurrence of that day of October 2026, moved to `hour`.
fn moved(day: u32, hour: u32, summary: &str) -> String {
    format!(
        "RECURRENCE-ID;TZID=Europe/Warsaw:202610{day}T090000\r\n\
         DTSTART;TZID=Europe/Warsaw:202610{day}T{hour}0000\r\n\
         DTEND;TZID=Europe/Warsaw:202610{day}T{}0000\r\nSUMMARY:{summary}",
        hour + 1
    )
}

/// The starts of the occurrences the service has from that day of October 2026 (UTC) for `days`.
async fn starts(s: &Stack, cal: &str, day: u32, days: u32) -> Vec<Value> {
    let id = cal.rsplit("/c-").next().unwrap();
    let (st, found) = s
        .rest(
            Method::GET,
            &format!(
                "/calendar/v1/calendars/{id}/events?from=2026-10-{day}T00%3A00%3A00Z&to=2026-10-{}T00%3A00%3A00Z",
                day + days
            ),
            ALICE,
            None,
        )
        .await;
    assert_eq!(st, StatusCode::OK);
    let all = found.as_array().unwrap().iter();
    all.map(|o| o["start"].clone()).collect()
}

#[tokio::test]
async fn changed_occurrence_round_trip() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let st = put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    assert_eq!(st, StatusCode::CREATED);

    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["rrule"], "FREQ=WEEKLY;COUNT=6");
    assert_eq!(parts[1]["uid"], "s1");
    assert_eq!(parts[1]["recurring_event_id"], parts[0]["id"]);
    assert_eq!(parts[1]["original_start"], "2026-10-12T09:00:00");
    assert_eq!(parts[1]["start"], "2026-10-12T11:00:00");
    assert_eq!(parts[1]["summary"], "Later");
    assert_eq!(starts(&s, &c, 12, 7).await, ["2026-10-12T11:00:00"]);

    let (st, _, ics) = get(&s, &c, "s1").await;
    assert_eq!(st, StatusCode::OK);
    let all = Event::all(&ics);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].val("RRULE"), Some("FREQ=WEEKLY;COUNT=6"));
    assert_eq!(all[0].val("RECURRENCE-ID"), None);
    assert_eq!(all[1].val("UID"), Some("s1"));
    assert_eq!(
        all[1].lines("RECURRENCE-ID"),
        [("TZID=Europe/Warsaw", "20261012T090000")]
    );
    assert_eq!(
        all[1].lines("DTSTART"),
        [("TZID=Europe/Warsaw", "20261012T110000")]
    );
    assert_eq!(all[1].val("SUMMARY"), Some("Later"));
    assert_eq!(all[1].val("RRULE"), None);

    // What the bridge wrote is the item as stored: sending it back writes nothing.
    let before = etag(&s, &c, "s1").await;
    let (st, _, _) = s
        .dav("PUT", &format!("{c}/s1.ics"), "alice", &[], &ics)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(etag(&s, &c, "s1").await, before);
}

#[tokio::test]
async fn adding_a_second_override_keeps_the_first() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    let first = stored(&s, &c, "s1").await;

    let st = put(
        &s,
        &c,
        &[SERIES, &moved(12, 11, "Later"), &moved(19, 14, "Latest")],
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 3);
    for (now, before) in parts.iter().zip(&first) {
        assert_eq!(now["id"], before["id"]);
        assert_eq!(now["etag"], before["etag"]);
    }
    assert_eq!(parts[2]["original_start"], "2026-10-19T09:00:00");
    assert_eq!(parts[2]["start"], "2026-10-19T14:00:00");
}

#[tokio::test]
async fn changing_an_override_changes_the_item_etag() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    let before = etag(&s, &c, "s1").await;
    let first = stored(&s, &c, "s1").await;

    let st = put(&s, &c, &[SERIES, &moved(12, 11, "Much later")]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_ne!(etag(&s, &c, "s1").await, before);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["etag"], first[0]["etag"]);
    assert_eq!(parts[1]["id"], first[1]["id"]);
    assert_ne!(parts[1]["etag"], first[1]["etag"]);
    assert_eq!(parts[1]["summary"], "Much later");
}

#[tokio::test]
async fn removing_an_override_restores_the_occurrence() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    let before = etag(&s, &c, "s1").await;

    assert_eq!(put(&s, &c, &[SERIES]).await, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &c, "s1").await.len(), 1);
    assert_ne!(etag(&s, &c, "s1").await, before);
    assert_eq!(starts(&s, &c, 12, 7).await, ["2026-10-12T09:00:00"]);
}

#[tokio::test]
async fn cancelled_occurrence_is_an_exdate() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let series = format!("{SERIES}\r\nEXDATE;TZID=Europe/Warsaw:20261019T090000");
    let st = put(&s, &c, &[&series, &moved(12, 11, "Later")]).await;
    assert_eq!(st, StatusCode::CREATED);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["exdates"], json!(["2026-10-19T09:00:00"]));
    assert!(starts(&s, &c, 19, 1).await.is_empty());
    assert_eq!(starts(&s, &c, 26, 1).await, ["2026-10-26T09:00:00"]);
}

#[tokio::test]
async fn all_day_series_with_override() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let series = "DTSTART;VALUE=DATE:20261005\r\nRRULE:FREQ=WEEKLY;COUNT=4";
    let changed = "RECURRENCE-ID;VALUE=DATE:20261012\r\nDTSTART;VALUE=DATE:20261013\r\nSUMMARY:x";
    assert_eq!(put(&s, &c, &[series, changed]).await, StatusCode::CREATED);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts[1]["original_start"], "2026-10-12");
    assert_eq!(parts[1]["start"], "2026-10-13");
    let (_, _, ics) = get(&s, &c, "s1").await;
    let all = Event::all(&ics);
    assert_eq!(all[1].lines("RECURRENCE-ID"), [("VALUE=DATE", "20261012")]);
}

#[tokio::test]
async fn deleting_the_item_deletes_everything() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "a"), &moved(19, 11, "b")]).await;
    assert_eq!(stored(&s, &c, "s1").await.len(), 3);
    let (st, _, _) = s
        .dav("DELETE", &format!("{c}/s1.ics"), "alice", &[], "")
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(stored(&s, &c, "s1").await.is_empty());
    assert_eq!(get(&s, &c, "s1").await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn listing_groups_parts_into_one_item() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "a"), &moved(19, 11, "b")]).await;
    let collection = format!("{c}/");
    let href = format!("{c}/s1.ics");

    let props = r#"<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:propfind>"#;
    let (_, _, xml) = s
        .dav("PROPFIND", &collection, "alice", &[("depth", "1")], props)
        .await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 2);
    assert_eq!(r[1].0, href);
    assert_eq!(text(r[1].1, DAV, "getetag"), etag(&s, &c, "s1").await);

    let multiget = format!(
        r#"<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
        <D:prop><D:getetag/><C:calendar-data/></D:prop><D:href>{href}</D:href></C:calendar-multiget>"#
    );
    let (st, _, xml) = s
        .dav("REPORT", &collection, "alice", &[("depth", "1")], &multiget)
        .await;
    assert_eq!(st, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 1);
    let data = text(r[0].1, CALDAV, "calendar-data");
    assert_eq!(data.matches("BEGIN:VCALENDAR").count(), 1);
    let all = Event::all(&data);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].val("RECURRENCE-ID"), None);
    assert_eq!(all[1].val("SUMMARY"), Some("a"));
    assert_eq!(all[2].val("SUMMARY"), Some("b"));
}

#[tokio::test]
async fn overrides_without_a_series_are_refused() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    refused(&s, &c, &[&moved(12, 11, "a")]).await;
    refused(&s, &c, &[&moved(12, 11, "a"), &moved(19, 11, "b")]).await;
    assert!(stored(&s, &c, "s1").await.is_empty());
}

#[tokio::test]
async fn this_and_future_is_refused() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let changed = moved(12, 11, "a").replace(";TZID=", ";RANGE=THISANDFUTURE;TZID=");
    assert!(changed.contains("RECURRENCE-ID;RANGE=THISANDFUTURE"));
    refused(&s, &c, &[SERIES, &changed]).await;
    assert!(stored(&s, &c, "s1").await.is_empty());
}

#[tokio::test]
async fn parts_with_different_uids_are_refused() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let changed = moved(12, 11, "a");
    let both = body(&[("s1", SERIES), ("s2", &changed)]);
    let (st, _, answer) = s
        .dav("PUT", &format!("{c}/s1.ics"), "alice", &[], &both)
        .await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(answer.contains("valid-calendar-data"), "{answer}");
    assert!(stored(&s, &c, "s1").await.is_empty());
}

/// Each of these would change when something happens, or cannot be stored.
#[tokio::test]
async fn malformed_overrides_are_refused() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let changed = moved(12, 11, "a");
    // The same occurrence twice, also when it is named in two ways.
    refused(&s, &c, &[SERIES, &changed, &moved(12, 13, "b")]).await;
    let in_utc = changed.replace(
        "RECURRENCE-ID;TZID=Europe/Warsaw:20261012T090000",
        "RECURRENCE-ID:20261012T070000Z",
    );
    refused(&s, &c, &[SERIES, &changed, &in_utc]).await;
    refused(&s, &c, &[SERIES, &format!("{changed}\r\nRRULE:FREQ=DAILY")]).await;
    let cancelled = format!("{changed}\r\nEXDATE;TZID=Europe/Warsaw:20261012T110000");
    refused(&s, &c, &[SERIES, &cancelled]).await;
    // A date names no occurrence of a timed series, and a time without a zone none at all.
    for id in [
        "RECURRENCE-ID;VALUE=DATE:20261012",
        "RECURRENCE-ID:20261012T090000",
    ] {
        let changed = format!("{id}\r\n{}", changed.split_once("\r\n").unwrap().1);
        refused(&s, &c, &[SERIES, &changed]).await;
    }
    // Two events without a RECURRENCE-ID are two events.
    refused(&s, &c, &[SERIES, SERIES]).await;
    assert!(stored(&s, &c, "s1").await.is_empty());
}

/// What the service would refuse only after the series is written, the bridge refuses before writing
/// anything: an override of an event that does not repeat, and one that is all-day in a timed series or
/// the reverse.
#[tokio::test]
async fn an_override_without_a_rule_is_refused_before_writing() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let single = SERIES.replace("RRULE:FREQ=WEEKLY;COUNT=6\r\n", "");
    let (st, answer) = put_as(&s, &c, &[&single, &moved(12, 11, "a")]).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(answer.contains("needs a repeating event"), "{answer}");
    assert!(stored(&s, &c, "s1").await.is_empty());

    let all_day = "RECURRENCE-ID;TZID=Europe/Warsaw:20261012T090000\r\nDTSTART;VALUE=DATE:20261012";
    let (st, answer) = put_as(&s, &c, &[SERIES, all_day]).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(answer.contains("all-day exactly when"), "{answer}");
    let series = "DTSTART;VALUE=DATE:20261005\r\nRRULE:FREQ=WEEKLY;COUNT=4";
    let timed = moved(12, 11, "a").replace(
        "RECURRENCE-ID;TZID=Europe/Warsaw:20261012T090000",
        "RECURRENCE-ID;VALUE=DATE:20261012",
    );
    refused(&s, &c, &[series, &timed]).await;
    assert!(stored(&s, &c, "s1").await.is_empty());

    // A stored item stays as it is.
    assert_eq!(put(&s, &c, &[SERIES]).await, StatusCode::CREATED);
    let before = etag(&s, &c, "s1").await;
    refused(&s, &c, &[&single, &moved(12, 11, "a")]).await;
    assert_eq!(etag(&s, &c, "s1").await, before);
}

/// A refusal only the service can make, here a summary that is too long, ends the write where it is: the
/// parts before it stay. Sending the corrected item again writes the rest and nothing twice.
#[tokio::test]
async fn retry_after_a_partial_write_converges() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let first = moved(12, 11, "a");
    let (st, answer) = put_as(&s, &c, &[SERIES, &first, &moved(19, 11, &"x".repeat(501))]).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(answer.contains("valid-calendar-data"), "{answer}");
    assert!(answer.contains("at most 500 characters"), "{answer}");
    let partial = stored(&s, &c, "s1").await;
    assert_eq!(partial.len(), 2);

    let st = put(&s, &c, &[SERIES, &first, &moved(19, 11, "b")]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 3);
    for (now, before) in parts.iter().zip(&partial) {
        assert_eq!(now["id"], before["id"]);
        assert_eq!(now["etag"], before["etag"]);
    }
    assert_eq!(parts[1]["original_start"], "2026-10-12T09:00:00");
    assert_eq!(parts[2]["original_start"], "2026-10-19T09:00:00");
    assert_eq!(parts[2]["summary"], "b");
}

#[tokio::test]
async fn if_match_on_a_multi_part_item() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    let before = etag(&s, &c, "s1").await;
    let id = stored(&s, &c, "s1").await[1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let changed = json!({"summary": "changed elsewhere", "all_day": false, "start": "2026-10-12T12:00:00",
        "end": "2026-10-12T13:00:00", "tz": "Europe/Warsaw"});
    let (st, _) = s
        .rest(
            Method::PUT,
            &format!("/calendar/v1/events/{id}"),
            ALICE,
            Some(changed),
        )
        .await;
    assert_eq!(st, StatusCode::OK);

    let path = format!("{c}/s1.ics");
    let item = body(&[("s1", SERIES)]);
    let (st, _, _) = s
        .dav("PUT", &path, "alice", &[("if-match", &before)], &item)
        .await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    assert_eq!(stored(&s, &c, "s1").await.len(), 2);
    let fresh = etag(&s, &c, "s1").await;
    assert_ne!(fresh, before);
    let (st, _, _) = s
        .dav("PUT", &path, "alice", &[("if-match", &fresh)], &item)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &c, "s1").await.len(), 1);
}

#[tokio::test]
async fn unchanged_all_day_series_is_not_rewritten() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let series = "DTSTART;VALUE=DATE:20261005\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\nSUMMARY:Weekly\r\n\
                  BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:x\r\nTRIGGER:-PT10M\r\nEND:VALARM";
    let changed = "RECURRENCE-ID;VALUE=DATE:20261012\r\nDTSTART;VALUE=DATE:20261013\r\nSUMMARY:x";
    assert_eq!(put(&s, &c, &[series, changed]).await, StatusCode::CREATED);
    let before = stored(&s, &c, "s1").await;
    assert_eq!(before[0]["reminders"], json!([10]));
    let tag = etag(&s, &c, "s1").await;

    let (_, _, ics) = get(&s, &c, "s1").await;
    let (st, _, _) = s
        .dav("PUT", &format!("{c}/s1.ics"), "alice", &[], &ics)
        .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(etag(&s, &c, "s1").await, tag);
    assert_eq!(stored(&s, &c, "s1").await, before);
}

#[tokio::test]
async fn single_event_becomes_a_series_with_overrides() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let single = SERIES.replace("RRULE:FREQ=WEEKLY;COUNT=6\r\n", "");
    assert_eq!(put(&s, &c, &[&single]).await, StatusCode::CREATED);
    let id = stored(&s, &c, "s1").await[0]["id"].clone();

    let st = put(&s, &c, &[SERIES, &moved(12, 11, "Later")]).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["id"], id);
    assert_eq!(parts[0]["rrule"], "FREQ=WEEKLY;COUNT=6");
    assert_eq!(parts[1]["recurring_event_id"], id);
    assert_eq!(starts(&s, &c, 12, 7).await, ["2026-10-12T11:00:00"]);

    // And back: the overrides go with the rule.
    assert_eq!(put(&s, &c, &[&single]).await, StatusCode::NO_CONTENT);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 1);
    assert_eq!(parts[0]["rrule"], Value::Null);
}

/// A changed occurrence made elsewhere is part of the item: it is sent, and it changes the etag.
#[tokio::test]
async fn override_made_elsewhere_shows() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    assert_eq!(put(&s, &c, &[SERIES]).await, StatusCode::CREATED);
    let before = etag(&s, &c, "s1").await;
    let series = stored(&s, &c, "s1").await[0]["id"].clone();
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

    let (_, h, ics) = get(&s, &c, "s1").await;
    assert_ne!(h["etag"], before.as_str());
    let all = Event::all(&ics);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].val("RRULE"), Some("FREQ=WEEKLY;COUNT=6"));
    assert_eq!(all[0].val("RECURRENCE-ID"), None);
    assert_eq!(
        all[1].lines("RECURRENCE-ID"),
        [("TZID=Europe/Warsaw", "20261012T090000")]
    );
    assert_eq!(all[1].val("UID"), Some("s1"));
    assert_eq!(all[1].val("SUMMARY"), Some("moved"));

    // One item in the listing, with the same etag.
    let props = r#"<D:propfind xmlns:D="DAV:"><D:prop><D:getetag/></D:prop></D:propfind>"#;
    let (_, _, xml) = s
        .dav(
            "PROPFIND",
            &format!("{c}/"),
            "alice",
            &[("depth", "1")],
            props,
        )
        .await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 2);
    assert_eq!(text(r[1].1, DAV, "getetag"), h["etag"].to_str().unwrap());
}

/// The clocks in Warsaw go back on 25 October 2026: 09:00 there is 07:00 UTC before and 08:00 UTC after.
#[tokio::test]
async fn utc_recurrence_id_matches_the_zoned_occurrence() {
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let series = format!("{SERIES}\r\nEXDATE:20261019T070000Z");
    let summer = "RECURRENCE-ID:20261012T070000Z\r\nDTSTART:20261012T090000Z\r\n\
                  DTEND:20261012T100000Z";
    let winter = "RECURRENCE-ID:20261026T080000Z\r\nDTSTART;TZID=Europe/Warsaw:20261026T120000\r\n\
                  DTEND;TZID=Europe/Warsaw:20261026T130000";
    let st = put(&s, &c, &[&series, summer, winter]).await;
    assert_eq!(st, StatusCode::CREATED);
    let parts = stored(&s, &c, "s1").await;
    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["exdates"], json!(["2026-10-19T09:00:00"]));
    assert_eq!(parts[1]["original_start"], "2026-10-12T09:00:00");
    // The occurrence itself keeps the zone it was sent in.
    assert_eq!(parts[1]["tz"], "UTC");
    assert_eq!(parts[1]["start"], "2026-10-12T09:00:00");
    assert_eq!(parts[2]["original_start"], "2026-10-26T09:00:00");

    // Both replace their occurrences, and the cancelled one is gone.
    assert_eq!(
        starts(&s, &c, 12, 15).await,
        ["2026-10-12T09:00:00", "2026-10-26T12:00:00"]
    );
    let (_, _, ics) = get(&s, &c, "s1").await;
    let all = Event::all(&ics);
    assert_eq!(
        all[1].lines("RECURRENCE-ID"),
        [("TZID=Europe/Warsaw", "20261012T090000")]
    );
    assert_eq!(all[1].lines("DTSTART"), [("", "20261012T090000Z")]);
}
