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

/// An empty body or `allprop` asks for everything. A DOCTYPE is refused, so no entity is ever expanded.
#[allow(clippy::result_large_err)] // the error type is the crate's own, as everywhere else
pub fn read_propfind(body: &str) -> Result<PropfindReq, DavError> {
    if body.trim().is_empty() {
        return Ok(PropfindReq { props: None });
    }
    let doc = roxmltree::Document::parse(body).map_err(|_| bad("malformed XML"))?;
    let root = doc.root_element();
    if root.tag_name().namespace() != Some(DAV) || root.tag_name().name() != "propfind" {
        return Err(bad("expected a propfind"));
    }
    let Some(prop) = root.children().find(|n| {
        n.is_element() && n.tag_name().name() == "prop" && n.tag_name().namespace() == Some(DAV)
    }) else {
        return Ok(PropfindReq { props: None });
    };
    let props = prop
        .children()
        .filter(|n| n.is_element())
        .map(|n| {
            let (ns, name) = (n.tag_name().namespace().unwrap_or(""), n.tag_name().name());
            match KNOWN
                .iter()
                .find(|(_, k_ns, k_name)| *k_ns == ns && *k_name == name)
            {
                Some((p, _, _)) => p.clone(),
                None => Prop::Unknown(ns.into(), name.into()),
            }
        })
        .collect();
    Ok(PropfindReq { props: Some(props) })
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
    #[test]
    fn escape_drops_what_xml_forbids() {
        assert_eq!(
            super::escape("a\u{1}b\u{b}\u{fffe}<\t\n&"),
            "ab&lt;\t\n&amp;"
        );
    }
}
