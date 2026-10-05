mod support;

use axum::http::{Method, StatusCode};
use roxmltree::{Document, Node};
use serde_json::json;
use support::{ALICE, Stack};

const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const CS: &str = "http://calendarserver.org/ns/";
const APPLE: &str = "http://apple.com/ns/ical/";

const COLLECTION_PROPS: &str = r#"<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:A="http://apple.com/ns/ical/">
  <D:prop><D:resourcetype/><D:owner/><D:displayname/><D:current-user-principal/>
  <D:current-user-privilege-set/><C:supported-calendar-component-set/><CS:getctag/><A:calendar-color/></D:prop>
</D:propfind>"#;

fn named<'a>(n: Node<'a, 'a>, ns: &str, name: &str) -> Option<Node<'a, 'a>> {
    n.descendants().find(|d| {
        d.is_element() && d.tag_name().name() == name && d.tag_name().namespace() == Some(ns)
    })
}

fn text(n: Node, ns: &str, name: &str) -> String {
    named(n, ns, name).unwrap().text().unwrap_or("").to_owned()
}

/// The `response` elements of a multistatus, keyed by their href.
fn responses<'a>(doc: &'a Document) -> Vec<(String, Node<'a, 'a>)> {
    doc.descendants()
        .filter(|n| n.tag_name().name() == "response" && n.tag_name().namespace() == Some(DAV))
        .map(|r| (text(r, DAV, "href"), r))
        .collect()
}

async fn propfind(s: &Stack, path: &str, depth: &str, body: &str) -> (StatusCode, String) {
    let (status, _, text) = s
        .dav("PROPFIND", path, "alice", &[("depth", depth)], body)
        .await;
    (status, text)
}

#[tokio::test]
async fn options_advertises_calendar_access() {
    let s = Stack::spawn().await;
    let (status, h, _) = s.dav("OPTIONS", "/dav/anything", "alice", &[], "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(h["dav"], "1, calendar-access");
    assert_eq!(h["allow"], "OPTIONS, PROPFIND, REPORT, GET, PUT, DELETE");
}

#[tokio::test]
async fn root_points_to_the_principal() {
    let s = Stack::spawn().await;
    let body =
        r#"<D:propfind xmlns:D="DAV:"><D:prop><D:current-user-principal/></D:prop></D:propfind>"#;
    let (status, xml) = propfind(&s, "/dav/", "0", body).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 1);
    let principal = named(r[0].1, DAV, "current-user-principal").unwrap();
    assert_eq!(text(principal, DAV, "href"), "/dav/principals/alice/");
}

#[tokio::test]
async fn principal_points_to_the_home() {
    let s = Stack::spawn().await;
    let body = r#"<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav"><D:prop><C:calendar-home-set/></D:prop></D:propfind>"#;
    let (status, xml) = propfind(&s, "/dav/principals/alice/", "0", body).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let home = named(doc.root_element(), CALDAV, "calendar-home-set").unwrap();
    assert_eq!(text(home, DAV, "href"), "/dav/calendars/alice/");
}

#[tokio::test]
async fn home_lists_collections() {
    let s = Stack::spawn().await;
    let (status, xml) = propfind(&s, "/dav/calendars/alice/", "1", COLLECTION_PROPS).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    assert_eq!(r.len(), 3);
    assert_eq!(r[0].0, "/dav/calendars/alice/");
    let find = |prefix: &str| r.iter().find(|(h, _)| h.contains(prefix)).unwrap();

    let (href, cal) = find("/c-");
    assert!(href.ends_with('/'));
    assert_eq!(text(*cal, DAV, "displayname"), "Personal");
    assert!(
        named(*cal, DAV, "resourcetype")
            .and_then(|t| named(t, CALDAV, "calendar"))
            .is_some()
    );
    let comp = named(*cal, CALDAV, "comp").unwrap();
    assert_eq!(comp.attribute("name"), Some("VEVENT"));
    assert!(text(*cal, APPLE, "calendar-color").starts_with('#'));
    assert!(text(*cal, CS, "getctag").parse::<i64>().is_ok());
    let owner = named(*cal, DAV, "owner").unwrap();
    assert_eq!(text(owner, DAV, "href"), "/dav/principals/alice/");

    let (_, list) = find("/t-");
    assert_eq!(text(*list, DAV, "displayname"), "Tasks");
    assert_eq!(
        named(*list, CALDAV, "comp").unwrap().attribute("name"),
        Some("VTODO")
    );
}

#[tokio::test]
async fn ctag_follows_the_sync_token() {
    let s = Stack::spawn().await;
    let body = r#"<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/"><D:prop><CS:getctag/></D:prop></D:propfind>"#;
    let ctag = |xml: String| {
        let doc = Document::parse(&xml).unwrap();
        let r = responses(&doc);
        let (href, node) = r.iter().find(|(h, _)| h.contains("/c-")).unwrap();
        (href.clone(), text(*node, CS, "getctag"))
    };
    let (_, xml) = propfind(&s, "/dav/calendars/alice/", "1", body).await;
    let (href, before) = ctag(xml);
    let id = href
        .trim_end_matches('/')
        .rsplit("/c-")
        .next()
        .unwrap()
        .to_owned();
    let event = json!({"summary": "x", "all_day": false, "start": "2026-10-05T10:00:00",
                       "end": "2026-10-05T11:00:00", "tz": "UTC"});
    let (st, _) = s
        .rest(
            Method::POST,
            &format!("/calendar/v1/calendars/{id}/events"),
            ALICE,
            Some(event),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, xml) = propfind(&s, "/dav/calendars/alice/", "1", body).await;
    assert_ne!(ctag(xml).1, before);
}

#[tokio::test]
async fn unknown_property_is_reported_missing() {
    let s = Stack::spawn().await;
    let body = r#"<D:propfind xmlns:D="DAV:"><D:prop><D:displayname/><D:quota-used-bytes/></D:prop></D:propfind>"#;
    let (status, xml) = propfind(&s, "/dav/calendars/alice/", "0", body).await;
    assert_eq!(status, StatusCode::MULTI_STATUS);
    let doc = Document::parse(&xml).unwrap();
    let stats: Vec<_> = doc
        .descendants()
        .filter(|n| n.tag_name().name() == "propstat")
        .map(|p| (text(p, DAV, "status"), p))
        .collect();
    assert_eq!(stats.len(), 2);
    let ok = stats.iter().find(|(s, _)| s.contains("200")).unwrap();
    assert!(named(ok.1, DAV, "displayname").is_some());
    let missing = stats.iter().find(|(s, _)| s.contains("404")).unwrap();
    assert!(named(missing.1, DAV, "quota-used-bytes").is_some());
}

#[tokio::test]
async fn other_users_path_is_404() {
    let s = Stack::spawn().await;
    let (status, _) = propfind(&s, "/dav/calendars/bob/", "0", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = propfind(&s, "/dav/principals/bob/", "0", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unknown_collection_is_404() {
    let s = Stack::spawn().await;
    let path = format!("/dav/calendars/alice/c-{}/", uuid::Uuid::new_v4());
    let (status, _) = propfind(&s, &path, "0", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn depth_infinity_is_403() {
    let s = Stack::spawn().await;
    let (status, _) = propfind(&s, "/dav/", "infinity", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn unsupported_method_is_405() {
    let s = Stack::spawn().await;
    for m in ["MKCALENDAR", "PROPPATCH"] {
        let (status, _, _) = s.dav(m, "/dav/calendars/alice/", "alice", &[], "").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{m}");
    }
}

#[tokio::test]
async fn doctype_is_refused() {
    let s = Stack::spawn().await;
    let body = r#"<!DOCTYPE x [<!ENTITY a "b">]><D:propfind xmlns:D="DAV:"><D:prop><D:displayname/></D:prop></D:propfind>"#;
    let (status, _) = propfind(&s, "/dav/", "0", body).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn empty_propfind_body_lists_all_properties() {
    let s = Stack::spawn().await;
    let (_, xml) = propfind(&s, "/dav/calendars/alice/", "1", "").await;
    let doc = Document::parse(&xml).unwrap();
    let r = responses(&doc);
    let (_, cal) = r.iter().find(|(h, _)| h.contains("/c-")).unwrap();
    for (ns, name) in [(DAV, "resourcetype"), (DAV, "displayname"), (CS, "getctag")] {
        assert!(named(*cal, ns, name).is_some(), "{name}");
    }
}

#[tokio::test]
async fn forbidden_characters_in_a_name_do_not_break_the_answer() {
    let s = Stack::spawn().await;
    let (st, _) = s
        .rest(
            Method::POST,
            "/calendar/v1/calendars",
            ALICE,
            Some(json!({"name": "a\u{1}b"})),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED);
    let (_, xml) = propfind(&s, "/dav/calendars/alice/", "1", COLLECTION_PROPS).await;
    let doc = Document::parse(&xml).unwrap();
    let names: Vec<_> = responses(&doc)
        .iter()
        .filter(|(h, _)| h.contains("/c-"))
        .map(|(_, r)| text(*r, DAV, "displayname"))
        .collect();
    assert!(names.contains(&"ab".to_owned()), "{names:?}");
}
