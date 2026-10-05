use axum::http::StatusCode;

use crate::DavError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prop {
    ResourceType,
    DisplayName,
    CurrentUserPrincipal,
    CalendarHomeSet,
    SupportedComponents,
    CTag,
    Color,
    SupportedReports,
    Privileges,
    Owner,
    ETag,
    ContentType,
    CalendarData,
    /// Namespace and local name.
    Unknown(String, String),
}

const DAV: &str = "DAV:";
const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
const CS: &str = "http://calendarserver.org/ns/";
const APPLE: &str = "http://apple.com/ns/ical/";

const KNOWN: [(Prop, &str, &str); 13] = [
    (Prop::ResourceType, DAV, "resourcetype"),
    (Prop::DisplayName, DAV, "displayname"),
    (Prop::CurrentUserPrincipal, DAV, "current-user-principal"),
    (Prop::CalendarHomeSet, CALDAV, "calendar-home-set"),
    (
        Prop::SupportedComponents,
        CALDAV,
        "supported-calendar-component-set",
    ),
    (Prop::CTag, CS, "getctag"),
    (Prop::Color, APPLE, "calendar-color"),
    (Prop::SupportedReports, DAV, "supported-report-set"),
    (Prop::Privileges, DAV, "current-user-privilege-set"),
    (Prop::Owner, DAV, "owner"),
    (Prop::ETag, DAV, "getetag"),
    (Prop::ContentType, DAV, "getcontenttype"),
    (Prop::CalendarData, CALDAV, "calendar-data"),
];

/// The properties a `PROPFIND` asks for; `None` is all of them.
pub struct PropfindReq {
    pub props: Option<Vec<Prop>>,
}

fn bad(message: &str) -> DavError {
    DavError::new(StatusCode::BAD_REQUEST, message)
}

fn is(n: roxmltree::Node, ns: &str, name: &str) -> bool {
    n.is_element() && n.tag_name().namespace() == Some(ns) && n.tag_name().name() == name
}

/// The properties named in the `prop` child of a request's root; `None` without one.
fn props(root: roxmltree::Node) -> Option<Vec<Prop>> {
    let prop = root.children().find(|n| is(*n, DAV, "prop"))?;
    let props = prop.children().filter(|n| n.is_element()).map(|n| {
        let (ns, name) = (n.tag_name().namespace().unwrap_or(""), n.tag_name().name());
        match KNOWN
            .iter()
            .find(|(_, k_ns, k_name)| *k_ns == ns && *k_name == name)
        {
            Some((p, _, _)) => p.clone(),
            None => Prop::Unknown(ns.into(), name.into()),
        }
    });
    Some(props.collect())
}

/// Nothing a client sends nests deeper or has a longer tag.
const MAX_DEPTH: usize = 32;
const MAX_TAG: usize = 8 * 1024;

/// Refuses a body the parser must not see: it recurses once per nested element, and a stack that runs out
/// ends the process; and it compares every attribute of a tag with every other one. Tags are told from
/// comments, CDATA sections, processing instructions and quoted attribute values as the parser tells them;
/// anything else that starts with `<!`, or does not end, is refused.
#[allow(clippy::result_large_err)] // the error type is the crate's own, as everywhere else
fn shape(body: &str) -> Result<(), DavError> {
    let b = body.as_bytes();
    // Where `end` first is at or after `from`, or a refusal.
    let after = |from: usize, end: &str| match body.get(from..).and_then(|rest| rest.find(end)) {
        Some(i) => Ok(from + i + end.len()),
        None => Err(bad("malformed XML")),
    };
    let (mut i, mut depth) = (0, 0usize);
    while let Some(start) = body[i..].find('<').map(|at| i + at) {
        let rest = &body[start..];
        if rest.starts_with("<!--") {
            i = after(start + 4, "-->")?;
        } else if rest.starts_with("<![CDATA[") {
            i = after(start + 9, "]]>")?;
        } else if rest.starts_with("<?") {
            i = after(start + 2, "?>")?;
        } else if rest.starts_with("<!") {
            return Err(bad("malformed XML"));
        } else {
            // To the `>` that is not inside an attribute's quotes.
            let mut quote = None;
            let mut end = start + 1;
            loop {
                match (b.get(end), quote) {
                    (None, _) => return Err(bad("malformed XML")),
                    _ if end - start >= MAX_TAG => return Err(bad("a tag is too long")),
                    (Some(b'>'), None) => break,
                    (Some(c @ (b'"' | b'\'')), None) => quote = Some(*c),
                    (Some(c), Some(q)) if *c == q => quote = None,
                    _ => {}
                }
                end += 1;
            }
            if b[start + 1] == b'/' {
                depth = depth.saturating_sub(1);
            } else {
                if depth == MAX_DEPTH {
                    return Err(bad("the XML is nested too deeply"));
                }
                if b[end - 1] != b'/' {
                    depth += 1;
                }
            }
            i = end + 1;
        }
    }
    Ok(())
}

/// The one place a request's XML is parsed. A DOCTYPE is refused, so no entity is ever expanded.
#[allow(clippy::result_large_err)] // as above
fn parse(body: &str) -> Result<roxmltree::Document<'_>, DavError> {
    shape(body)?;
    roxmltree::Document::parse(body).map_err(|_| bad("malformed XML"))
}

/// An empty body or `allprop` asks for everything.
#[allow(clippy::result_large_err)] // as above
pub fn read_propfind(body: &str) -> Result<PropfindReq, DavError> {
    if body.trim().is_empty() {
        return Ok(PropfindReq { props: None });
    }
    let doc = parse(body)?;
    let root = doc.root_element();
    if !is(root, DAV, "propfind") {
        return Err(bad("expected a propfind"));
    }
    Ok(PropfindReq { props: props(root) })
}

pub enum ReportReq {
    /// The hrefs as the client wrote them.
    Multiget {
        props: Vec<Prop>,
        hrefs: Vec<String>,
    },
    /// Its filter is not read: the answer is every item.
    Query { props: Vec<Prop> },
}

/// Without a `prop`, a report is answered with the etag and the data.
#[allow(clippy::result_large_err)] // as above
pub fn read_report(body: &str) -> Result<ReportReq, DavError> {
    let doc = parse(body)?;
    let root = doc.root_element();
    let props = props(root).unwrap_or_else(|| vec![Prop::ETag, Prop::CalendarData]);
    if is(root, CALDAV, "calendar-multiget") {
        let hrefs = root
            .children()
            .filter(|n| is(*n, DAV, "href"))
            .map(|n| n.text().unwrap_or("").trim().to_owned())
            .collect();
        Ok(ReportReq::Multiget { props, hrefs })
    } else if is(root, CALDAV, "calendar-query") {
        Ok(ReportReq::Query { props })
    } else {
        Err(DavError::new(StatusCode::FORBIDDEN, "report not supported"))
    }
}

/// Escapes text for XML 1.0; characters the format forbids are dropped.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            '\0'..='\u{1f}' | '\u{fffe}' | '\u{ffff}' => {}
            _ => out.push(c),
        }
    }
    out
}

pub struct Response {
    pub href: String,
    /// Inner XML of each property.
    pub found: Vec<(Prop, String)>,
    pub missing: Vec<Prop>,
    /// Nothing is at `href`: the answer is that status alone.
    pub not_found: bool,
}

/// The element for a property, empty or around `inner`, with the fixed prefixes of `multistatus`.
fn element(prop: &Prop, inner: Option<&str>) -> String {
    let (tag, xmlns) = match prop {
        Prop::Unknown(ns, name) => (name.clone(), format!(" xmlns=\"{}\"", escape(ns))),
        known => {
            let (_, ns, name) = KNOWN.iter().find(|(p, _, _)| p == known).unwrap();
            let prefix = match *ns {
                DAV => "D",
                CALDAV => "C",
                CS => "CS",
                _ => "A",
            };
            (format!("{prefix}:{name}"), String::new())
        }
    };
    match inner {
        Some(inner) => format!("<{tag}{xmlns}>{inner}</{tag}>"),
        None => format!("<{tag}{xmlns}/>"),
    }
}

/// The body of a 207 answer.
pub fn multistatus(responses: &[Response]) -> String {
    let mut out = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"{DAV}\" xmlns:C=\"{CALDAV}\" xmlns:CS=\"{CS}\" xmlns:A=\"{APPLE}\">"
    );
    for r in responses {
        out += &format!("<D:response><D:href>{}</D:href>", escape(&r.href));
        if r.not_found {
            out += "<D:status>HTTP/1.1 404 Not Found</D:status></D:response>";
            continue;
        }
        if !r.found.is_empty() {
            out += "<D:propstat><D:prop>";
            for (p, inner) in &r.found {
                out += &element(p, Some(inner));
            }
            out += "</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>";
        }
        if !r.missing.is_empty() {
            out += "<D:propstat><D:prop>";
            for p in &r.missing {
                out += &element(p, None);
            }
            out += "</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>";
        }
        out += "</D:response>";
    }
    out + "</D:multistatus>"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(body: &str) -> bool {
        shape(body).is_err_and(|e| e.status() == StatusCode::BAD_REQUEST)
    }

    fn nested(depth: usize) -> String {
        format!("{}{}", "<a>".repeat(depth), "</a>".repeat(depth))
    }

    #[test]
    fn nesting_is_limited() {
        assert!(shape(&nested(32)).is_ok());
        assert!(refused(&nested(33)));
        // An empty element is one level too.
        assert!(shape(&format!("{}<b/>", "<a>".repeat(31))).is_ok());
        assert!(refused(&format!("{}<b/>", "<a>".repeat(32))));
        // Elements beside each other are not nested.
        assert!(shape(&format!("<r>{}</r>", "<a><b/></a>".repeat(100))).is_ok());
    }

    #[test]
    fn quoted_angle_brackets_are_not_tags() {
        // Each attribute value would otherwise end the tag early, as an empty element.
        for attribute in [r#"x="/>""#, "x='/>'", r#"x=">" y='"/>'"#] {
            let open = format!("<a {attribute}>");
            assert!(shape(&format!("{}<b/>", open.repeat(31))).is_ok());
            assert!(refused(&format!("{}<b/>", open.repeat(32))), "{attribute}");
        }
        assert!(shape(&format!("{}<b/>", r#"<a x="/>"/>"#.repeat(40))).is_ok());
    }

    #[test]
    fn comments_cdata_and_instructions_are_skipped() {
        let deep = "<a>".repeat(40);
        for skipped in [
            format!("<!-- {deep} -->"),
            format!("<![CDATA[{deep}]]>"),
            format!("<?pi {deep} ?>"),
        ] {
            assert!(shape(&format!("<r>{skipped}</r>")).is_ok(), "{skipped}");
            // What follows is counted again.
            assert!(refused(&format!("{skipped}{}", nested(33))));
        }
        for open in [
            "<r><!-- ",
            "<r><![CDATA[",
            "<r><?pi ",
            "<r><a x=\"",
            "<r",
            "<",
        ] {
            assert!(refused(open), "{open}");
        }
        assert!(refused("<!DOCTYPE x><r/>"));
    }

    #[test]
    fn a_tag_is_limited_in_length() {
        let tag = |len: usize| format!("<a x=\"{}\"/>", "y".repeat(len - 9));
        assert_eq!(tag(8192).len(), 8192);
        assert!(shape(&tag(8192)).is_ok());
        assert!(refused(&tag(8193)));
        assert!(refused(&format!("<a></a{}>", " ".repeat(8192))));
        // Text and comments are not tags.
        assert!(shape(&format!("<a>{0}<!--{0}--></a>", "y".repeat(100_000))).is_ok());
    }

    #[test]
    fn what_thunderbird_sends_passes() {
        let propfind = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:resourcetype/><D:owner/><D:displayname/><D:current-user-principal/>
  <D:current-user-privilege-set/><C:supported-calendar-component-set/><CS:getctag/></D:prop>
</D:propfind>"#;
        let asked = read_propfind(propfind).ok().unwrap().props.unwrap();
        assert_eq!(asked.len(), 7);

        let hrefs: String = (0..5000)
            .map(|i| format!("<D:href>/dav/calendars/alice/c-1/{i}.ics</D:href>\n"))
            .collect();
        let multiget = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
{hrefs}</C:calendar-multiget>"#
        );
        assert!(shape(&multiget).is_ok());
        let Ok(ReportReq::Multiget { hrefs, .. }) = read_report(&multiget) else {
            panic!("not read as a multiget");
        };
        assert_eq!(hrefs.len(), 5000);

        let query = r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VEVENT">
    <C:time-range start="20260101T000000Z" end="20270101T000000Z"/>
  </C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#;
        assert!(matches!(read_report(query), Ok(ReportReq::Query { .. })));
    }

    #[test]
    fn escape_drops_what_xml_forbids() {
        assert_eq!(escape("a\u{1}b\u{b}\u{fffe}<\t\n&"), "ab&lt;\t\n&amp;");
    }
}
