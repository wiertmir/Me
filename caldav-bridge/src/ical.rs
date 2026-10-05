//! iCalendar text to and from what the services store. Parsing, text escaping and line folding are the
//! `icalendar` crate's; reading times and alarms and writing lines are here, for to-dos and events alike.
#![allow(clippy::result_large_err)] // the error type is the crate's own, as everywhere else

use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use common::time::{When, parse_tz};
use icalendar::parser::{Component, Property, read_components, unfold};

use crate::{
    DavError,
    backend::{Task, TaskWrite},
};

/// The service's limits for reminders: how many, and how long before.
const MAX_REMINDERS: usize = 5;
const MAX_REMINDER_MINUTES: u64 = 40_320;

fn invalid(message: impl Into<String>) -> DavError {
    DavError::new(StatusCode::FORBIDDEN, message).precondition("valid-calendar-data")
}

fn unsupported(message: &str) -> DavError {
    DavError::new(StatusCode::FORBIDDEN, message).precondition("supported-calendar-data")
}

// ---- reading ----

fn is(c: &Component, name: &str) -> bool {
    c.name.as_str().eq_ignore_ascii_case(name)
}

fn prop<'a>(c: &'a Component, name: &str) -> Option<&'a Property<'a>> {
    c.properties
        .iter()
        .find(|p| p.name.as_str().eq_ignore_ascii_case(name))
}

fn param<'a>(p: &'a Property, name: &str) -> Option<&'a str> {
    let found = p
        .params
        .iter()
        .find(|q| q.key.as_str().eq_ignore_ascii_case(name))?;
    Some(found.val.as_ref()?.as_str().trim_matches('"'))
}

/// A property's value, unescaped by the parser; empty without the property.
fn text(c: &Component, name: &str) -> String {
    prop(c, name).map_or_else(String::new, |p| p.val.as_str().to_owned())
}

/// A body ready for `components`: line ends made CRLF, then unfolded. The parser looks for CRLF first, so
/// with bare LFs it reads each value to the next CRLF or, with none, scans the rest of the body per line.
fn unfolded(body: &str) -> String {
    unfold(&body.replace("\r\n", "\n").replace('\n', "\r\n"))
}

/// What the one `VCALENDAR` of a body holds, its `VTIMEZONE`s left out. `unfolded` is the body after
/// `unfolded()`.
fn components(unfolded: &str) -> Result<Vec<Component<'_>>, DavError> {
    // The parser recurses once per nested component, and a stack that runs out ends the process. Nothing
    // real nests deeper than an alarm in a to-do or a rule in a zone.
    let mut depth = 0usize;
    for line in unfolded.lines() {
        let starts = |tag: &str| {
            line.get(..tag.len())
                .is_some_and(|l| l.eq_ignore_ascii_case(tag))
        };
        if starts("BEGIN:") {
            depth += 1;
        } else if starts("END:") {
            depth = depth.saturating_sub(1);
        }
        if depth > 8 {
            return Err(invalid("components are nested too deeply"));
        }
    }
    let mut roots = read_components(unfolded).map_err(|_| invalid("the body is not iCalendar"))?;
    match roots.pop() {
        Some(root) if roots.is_empty() && is(&root, "VCALENDAR") => Ok(root
            .components
            .into_iter()
            .filter(|c| !is(c, "VTIMEZONE"))
            .collect()),
        _ => Err(invalid("the body is not one VCALENDAR")),
    }
}

/// A `DATE` or `DATE-TIME` property as the services take it: a date without a zone, or a wall-clock time
/// with one (`UTC` for a value ending in `Z`). A time with neither `Z` nor a known `TZID` is refused.
fn when(p: &Property) -> Result<(When, Option<String>), DavError> {
    let (name, value) = (p.name.as_str(), p.val.as_str().trim());
    let malformed = |_| invalid(format!("{name} is not a date or a date-time"));
    let timed = |v| NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S").map(When::Timed);
    let date = param(p, "VALUE").is_some_and(|v| v.eq_ignore_ascii_case("DATE"));
    if date || !value.contains(['T', 't']) {
        let d = NaiveDate::parse_from_str(value, "%Y%m%d").map_err(malformed)?;
        Ok((When::Date(d), None))
    } else if let Some(utc) = value.strip_suffix(['Z', 'z']) {
        Ok((timed(utc).map_err(malformed)?, Some("UTC".into())))
    } else {
        let tz = param(p, "TZID")
            .ok_or_else(|| invalid(format!("{name} has neither a time zone nor a Z")))?;
        parse_tz(tz).map_err(|_| invalid(format!("{name} names an unknown time zone")))?;
        Ok((timed(value).map_err(malformed)?, Some(tz.to_owned())))
    }
}

/// The minutes of a duration that is zero or negative and in whole minutes, such as `-PT15M` or `-P1DT2H`.
fn minutes_before(duration: &str) -> Option<u64> {
    let (negative, rest) = match duration.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, duration.strip_prefix('+').unwrap_or(duration)),
    };
    let (mut seconds, mut n) = (0u64, 0u64);
    for c in rest.strip_prefix('P')?.chars() {
        let unit = match c {
            '0'..='9' => {
                n = n.checked_mul(10)?.checked_add(c as u64 - '0' as u64)?;
                continue;
            }
            'T' => continue,
            'W' => 604_800,
            'D' => 86_400,
            'H' => 3_600,
            'M' => 60,
            'S' => 1,
            _ => return None,
        };
        seconds = seconds.checked_add(n.checked_mul(unit)?)?;
        n = 0;
    }
    (n == 0 && (negative || seconds == 0) && seconds % 60 == 0).then_some(seconds / 60)
}

/// The reminders in a component's alarms: those with a `TRIGGER` relative to the start (or, for a to-do,
/// which has only its `DUE`, to the end as well) that the service can hold. Every other alarm is dropped.
fn reminders(c: &Component, end_too: bool) -> Vec<u32> {
    let minutes = |alarm: &Component| {
        let trigger = prop(alarm, "TRIGGER")?;
        if !end_too && param(trigger, "RELATED").is_some_and(|r| r.eq_ignore_ascii_case("END")) {
            return None;
        }
        let m = minutes_before(trigger.val.as_str().trim())?;
        (m <= MAX_REMINDER_MINUTES).then_some(m as u32)
    };
    let alarms = c.components.iter().filter(|a| is(a, "VALARM"));
    alarms.filter_map(minutes).take(MAX_REMINDERS).collect()
}

// ---- writing ----

/// One content line: text escaped and the line folded by the crate, control characters dropped.
fn line(name: &str, param: Option<(&str, &str)>, value: &str) -> String {
    let value: String = value
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let mut p = icalendar::Property::new(name, value);
    if let Some((key, val)) = param {
        p.add_parameter(key, val);
    }
    p.try_into().expect("writing to a String cannot fail")
}

fn utc(name: &str, t: DateTime<Utc>) -> String {
    line(name, None, &t.format("%Y%m%dT%H%M%SZ").to_string())
}

/// A date or a time as the services give it (`2026-10-07`, or `2026-10-07T09:00:00` with a zone).
fn when_line(name: &str, value: &str, tz: Option<&str>) -> String {
    let value = value.replace(['-', ':'], "");
    let param = tz.map_or(("VALUE", "DATE"), |tz| ("TZID", tz));
    line(name, Some(param), &value)
}

/// `related_end`: the alarm counts from the component's end, a to-do's `DUE`.
fn alarm(minutes: u32, related_end: bool) -> String {
    let related = related_end.then_some(("RELATED", "END"));
    let trigger = line("TRIGGER", related, &format!("-PT{minutes}M"));
    format!("BEGIN:VALARM\r\nACTION:DISPLAY\r\nDESCRIPTION:Reminder\r\n{trigger}END:VALARM\r\n")
}

fn calendar(components: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Me//caldav-bridge//EN\r\n{components}END:VCALENDAR\r\n"
    )
}

// ---- to-dos ----

/// A to-do a client sent.
pub struct TodoIn {
    pub uid: String,
    /// Without a `parent_id`: `related_to` is for the caller to look up.
    pub write: TaskWrite,
    /// The uid of the parent it names.
    pub related_to: Option<String>,
}

/// The repeat rule is not written: every task is a plain to-do to the client.
pub fn todo_to_ical(t: &Task, parent_uid: Option<&str>) -> String {
    let mut out = String::from("BEGIN:VTODO\r\n");
    out += &line("UID", None, &t.uid);
    out += &utc("DTSTAMP", t.updated_at);
    out += &utc("LAST-MODIFIED", t.updated_at);
    out += &line("SUMMARY", None, &t.summary);
    if !t.description.is_empty() {
        out += &line("DESCRIPTION", None, &t.description);
    }
    if let Some(due) = &t.due {
        out += &when_line("DUE", due, t.tz.as_deref());
    }
    out += &line("PRIORITY", None, &t.priority.to_string());
    if t.completed {
        out += "STATUS:COMPLETED\r\nPERCENT-COMPLETE:100\r\n";
    } else {
        out += "STATUS:NEEDS-ACTION\r\n";
    }
    if let Some(at) = t.completed_at {
        out += &utc("COMPLETED", at);
    }
    if let Some(parent) = parent_uid {
        out += &line("RELATED-TO", None, parent);
    }
    for minutes in &t.reminders {
        out += &alarm(*minutes, true);
    }
    calendar(&(out + "END:VTODO\r\n"))
}

pub fn ical_to_todo(body: &str) -> Result<TodoIn, DavError> {
    let text_of_body = unfolded(body);
    let parts = components(&text_of_body)?;
    let todo = match &parts[..] {
        [one] if is(one, "VTODO") => one,
        _ => return Err(unsupported("a task list takes one VTODO per item")),
    };
    let uid = text(todo, "UID");
    if uid.is_empty() {
        return Err(invalid("the VTODO has no UID"));
    }
    let (due, tz) = match prop(todo, "DUE").map(when).transpose()? {
        Some((due, tz)) => (Some(due.to_string()), tz),
        None => (None, None),
    };
    let completed = text(todo, "STATUS").eq_ignore_ascii_case("COMPLETED")
        || prop(todo, "COMPLETED").is_some()
        || text(todo, "PERCENT-COMPLETE").trim() == "100";
    let priority = text(todo, "PRIORITY").trim().parse().ok();
    // A relation of another kind (a child, a sibling) names no parent.
    let related_to = todo.properties.iter().find(|p| {
        p.name.as_str().eq_ignore_ascii_case("RELATED-TO")
            && param(p, "RELTYPE").is_none_or(|r| r.eq_ignore_ascii_case("PARENT"))
            && !p.val.as_str().trim().is_empty()
    });
    let write = TaskWrite {
        uid: Some(uid.clone()),
        summary: text(todo, "SUMMARY"),
        description: text(todo, "DESCRIPTION"),
        priority: priority.filter(|p| *p <= 9).unwrap_or(0),
        completed,
        // The service takes reminders only with a due.
        reminders: if due.is_some() {
            reminders(todo, true)
        } else {
            Vec::new()
        },
        parent_id: None,
        // Nor a rule: without a due the to-do is stored as a plain one.
        rrule: prop(todo, "RRULE")
            .filter(|_| due.is_some())
            .map(|p| p.val.as_str().to_owned()),
        due,
        tz,
    };
    Ok(TodoIn {
        uid,
        write,
        related_to: related_to.map(|p| p.val.as_str().to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        for (text, minutes) in [
            ("-PT15M", Some(15)),
            ("-PT1H30M", Some(90)),
            ("-P1DT2H", Some(1560)),
            ("-P1W", Some(10_080)),
            ("PT0S", Some(0)),
            ("-PT120S", Some(2)),
            ("PT5M", None),
            ("-PT90S", None),
            ("20261007T060000Z", None),
            ("-PT99999999999999999999M", None),
            ("-PT5", None),
            ("", None),
        ] {
            assert_eq!(minutes_before(text), minutes, "{text}");
        }
    }

    #[test]
    fn any_line_ends_parse() {
        let lf =
            "BEGIN:VCALENDAR\nBEGIN:VTODO\nUID:t1\nSUMMARY:one\n two\nEND:VTODO\nEND:VCALENDAR\n";
        let mixed = "BEGIN:VCALENDAR\r\nBEGIN:VTODO\nUID:t1\nSUMMARY:one\r\n two\nDESCRIPTION:d\r\n\
                     END:VTODO\nEND:VCALENDAR\r\n";
        for body in [lf, mixed] {
            let Ok(todo) = ical_to_todo(body) else {
                panic!("{body:?} is refused");
            };
            assert_eq!(todo.uid, "t1");
            assert_eq!(todo.write.summary, "onetwo");
        }
    }

    #[test]
    fn deep_nesting_is_refused_before_it_is_parsed() {
        let body = format!(
            "{}{}",
            "BEGIN:A\r\n".repeat(200_000),
            "END:A\r\n".repeat(200_000)
        );
        assert!(ical_to_todo(&body).is_err());
    }

    #[test]
    fn control_characters_cannot_start_a_line() {
        let l = line("SUMMARY", None, "a\r\nEND:VTODO\u{0}");
        assert_eq!(l, "SUMMARY:a\\nEND:VTODO\r\n");
    }
}
