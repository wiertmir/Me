use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_decode_str, utf8_percent_encode};
use uuid::Uuid;

/// Events live in `c-` collections, to-dos in `t-` ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Events,
    Todos,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Root,
    Principal,
    Home,
    Collection(Kind, Uuid),
    /// The uid, decoded and without `.ics`.
    Item(Kind, Uuid, String),
}

const SEGMENT: AsciiSet = NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode(s: &str) -> String {
    utf8_percent_encode(s, &SEGMENT).to_string()
}

fn collection(segment: &str) -> Option<(Kind, Uuid)> {
    let (kind, id) = match segment.split_at_checked(2)? {
        ("c-", id) => (Kind::Events, id),
        ("t-", id) => (Kind::Todos, id),
        _ => return None,
    };
    Some((kind, id.parse().ok()?))
}

/// The target of a request path (percent-encoded, with the `/dav` prefix); `None` for a path that names
/// nothing here, another user's name included.
pub fn parse(path: &str, username: &str) -> Option<Target> {
    let rest = path.strip_prefix("/dav")?;
    if !rest.is_empty() && !rest.starts_with('/') {
        return None;
    }
    let mut segments = Vec::new();
    for s in rest.split('/').filter(|s| !s.is_empty()) {
        segments.push(percent_decode_str(s).decode_utf8().ok()?.into_owned());
    }
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    match segments[..] {
        [] => Some(Target::Root),
        ["principals", u] if u == username => Some(Target::Principal),
        ["calendars", u] if u == username => Some(Target::Home),
        ["calendars", u, c] if u == username => {
            let (kind, id) = collection(c)?;
            Some(Target::Collection(kind, id))
        }
        ["calendars", u, c, item] if u == username => {
            let (kind, id) = collection(c)?;
            let uid = item.strip_suffix(".ics").filter(|u| !u.is_empty())?;
            Some(Target::Item(kind, id, uid.to_owned()))
        }
        _ => None,
    }
}

pub fn href(target: &Target, username: &str) -> String {
    let user = encode(username);
    let prefix = |kind| match kind {
        Kind::Events => "c-",
        Kind::Todos => "t-",
    };
    match target {
        Target::Root => "/dav/".into(),
        Target::Principal => format!("/dav/principals/{user}/"),
        Target::Home => format!("/dav/calendars/{user}/"),
        Target::Collection(k, id) => format!("/dav/calendars/{user}/{}{id}/", prefix(*k)),
        Target::Item(k, id, uid) => {
            format!(
                "/dav/calendars/{user}/{}{id}/{}.ics",
                prefix(*k),
                encode(uid)
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_round_trip() {
        let id = Uuid::new_v4();
        for t in [
            Target::Root,
            Target::Principal,
            Target::Home,
            Target::Collection(Kind::Todos, id),
            Target::Item(Kind::Events, id, "a b/ü@x".into()),
        ] {
            assert_eq!(parse(&href(&t, "al ice"), "al ice"), Some(t));
        }
        assert_eq!(parse("/davprincipals/alice", "alice"), None);
        assert_eq!(parse("/dav/calendars/bob/", "alice"), None);
        assert_eq!(parse("/dav/calendars/alice/c-nope/", "alice"), None);
    }
}
