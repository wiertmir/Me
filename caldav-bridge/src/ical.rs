//! iCalendar text to and from what the services store. Parsing, text escaping and line folding are the
//! `icalendar` crate's; reading times and alarms and writing lines are here, for to-dos and events alike.
#![allow(clippy::result_large_err)] // the error type is the crate's own, as everywhere else

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use common::time::{When, parse_tz, to_utc};
use icalendar::parser::{Component, Property, read_components, unfold};

use crate::{
    DavError,
    backend::{Event, EventWrite, Task, TaskWrite},
};

/// The service's limits for reminders: how many, and how long before.
const MAX_REMINDERS: usize = 5;
const MAX_REMINDER_MINUTES: u64 = 40_320;
/// Each changed occurrence of an item is a write to the service of its own.
const MAX_OVERRIDES: usize = 500;

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

/// Every property of that name, for those that may come more than once.
fn props<'a>(c: &'a Component<'a>, name: &'a str) -> impl Iterator<Item = &'a Property<'a>> {
    let named = move |p: &&Property| p.name.as_str().eq_ignore_ascii_case(name);
    c.properties.iter().filter(named)
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
    when_of(p, p.val.as_str())
}

/// `when` for one `value` of a property that may hold several, such as `EXDATE`.
fn when_of(p: &Property, value: &str) -> Result<(When, Option<String>), DavError> {
    let (name, value) = (p.name.as_str(), value.trim());
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

/// The number before `unit` at the start of `rest`, taken off it; `None`, and `rest` as it was, without one.
fn number(rest: &mut &str, unit: char) -> Option<u64> {
    let digits = rest.find(|c: char| !c.is_ascii_digit())?;
    let n = rest[..digits].parse().ok()?;
    *rest = rest[digits..].strip_prefix(unit)?;
    Some(n)
}

/// A duration without its sign, in the forms RFC 5545 has and no other: `PnW`, or `P[nD][T[nH][nM][nS]]`
/// with at least one part. Its days, which are counted on the wall clock, and the seconds of its time part,
/// which are real time.
fn duration(text: &str) -> Option<(u64, u64)> {
    let mut rest = text.strip_prefix('P')?;
    if let Some(weeks) = number(&mut rest, 'W') {
        return rest.is_empty().then_some((weeks.checked_mul(7)?, 0));
    }
    let days = number(&mut rest, 'D');
    let mut seconds = None;
    if let Some(time) = rest.strip_prefix('T') {
        rest = time;
        for (unit, length) in [('H', 3_600), ('M', 60), ('S', 1)] {
            if let Some(n) = number(&mut rest, unit) {
                let so_far: u64 = seconds.unwrap_or(0);
                seconds = Some(so_far.checked_add(n.checked_mul(length)?)?);
            }
        }
        seconds?;
    }
    (rest.is_empty() && (days.is_some() || seconds.is_some()))
        .then_some((days.unwrap_or(0), seconds.unwrap_or(0)))
}

/// The minutes of a duration that is zero or negative and in whole minutes, such as `-PT15M` or `-P1DT2H`.
fn minutes_before(trigger: &str) -> Option<u64> {
    let (negative, rest) = match trigger.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trigger.strip_prefix('+').unwrap_or(trigger)),
    };
    let (days, seconds) = duration(rest)?;
    let seconds = days.checked_mul(86_400)?.checked_add(seconds)?;
    ((negative || seconds == 0) && seconds % 60 == 0).then_some(seconds / 60)
}

/// One value of a date or time property in the form of the event's `DTSTART`: a date for an all-day event,
/// otherwise the wall-clock time in the start's zone `tz`, whatever zone the value itself is written in.
/// An event is stored with one zone, so its end, its cancelled occurrences and the occurrence an override
/// replaces are all read this way.
fn in_form_of_start(p: &Property, value: &str, tz: Option<&str>) -> Result<String, DavError> {
    let zone = |name: &str| parse_tz(name).map_err(|_| invalid("unknown time zone"));
    let converted = match (when_of(p, value)?, tz) {
        ((date @ When::Date(_), _), None) => date,
        ((When::Timed(t), Some(from)), Some(to)) if from == to => When::Timed(t),
        ((When::Timed(t), Some(from)), Some(to)) => When::Timed(
            to_utc(t, zone(&from)?)
                .with_timezone(&zone(to)?)
                .naive_local(),
        ),
        _ => {
            let name = p.name.as_str();
            return Err(invalid(format!("{name} is not in the form of DTSTART")));
        }
    };
    Ok(converted.to_string())
}

/// The start, in the zone `tz`, with a positive `DURATION` added: its days on the wall clock, its time part
/// in real time, so that `PT8H` is eight hours also across a change of the clocks. For a date, whole days.
fn after(start: When, tz: Option<&str>, length: &str) -> Option<When> {
    let (days, seconds) = duration(length.strip_prefix('+').unwrap_or(length))?;
    let days = Duration::try_days(i64::try_from(days).ok()?)?;
    let seconds = Duration::try_seconds(i64::try_from(seconds).ok()?)?;
    Some(match start {
        When::Date(d) => When::Date(d.checked_add_signed(days)?.checked_add_signed(seconds)?),
        When::Timed(t) => {
            let day = t.checked_add_signed(days)?;
            if seconds.is_zero() {
                return Some(When::Timed(day));
            }
            let zone = parse_tz(tz?).ok()?;
            let end = to_utc(day, zone).checked_add_signed(seconds)?;
            When::Timed(end.with_timezone(&zone).naive_local())
        }
    })
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

/// A date or a time as the services give it (`2026-10-07`, or `2026-10-07T09:00:00` with a zone). A time in
/// `UTC` is written with a `Z`, which a client reads without looking a zone up.
fn when_line(name: &str, value: &str, tz: Option<&str>) -> String {
    let value = value.replace(['-', ':'], "");
    match tz {
        Some("UTC") => line(name, None, &format!("{value}Z")),
        Some(tz) => line(name, Some(("TZID", tz)), &value),
        None => line(name, Some(("VALUE", "DATE")), &value),
    }
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

// ---- events ----

/// An item a client sent: everything under one uid.
pub struct ItemIn {
    pub uid: String,
    /// The single event or the series.
    pub main: EventWrite,
    /// Its changed occurrences, each with the `original_start` it replaces and without the series' id.
    pub overrides: Vec<EventWrite>,
}

/// `parts[0]` is the single event or the series; its overrides follow.
pub fn events_to_ical(parts: &[Event]) -> String {
    // An override names its occurrence in the form of the series' start, whatever zone it has itself.
    let series_tz = parts.first().and_then(|e| e.tz.as_deref());
    let mut out = String::new();
    for e in parts {
        let tz = e.tz.as_deref();
        out += "BEGIN:VEVENT\r\n";
        out += &line("UID", None, &e.uid);
        out += &utc("DTSTAMP", e.updated_at);
        out += &utc("LAST-MODIFIED", e.updated_at);
        if let Some(original) = &e.original_start {
            out += &when_line("RECURRENCE-ID", original, series_tz);
        }
        out += &line("SUMMARY", None, &e.summary);
        if !e.description.is_empty() {
            out += &line("DESCRIPTION", None, &e.description);
        }
        if !e.location.is_empty() {
            out += &line("LOCATION", None, &e.location);
        }
        out += &when_line("DTSTART", &e.start, tz);
        out += &when_line("DTEND", &e.end, tz);
        if let Some(rule) = &e.rrule {
            out += &line("RRULE", None, rule);
        }
        for cancelled in &e.exdates {
            out += &when_line("EXDATE", cancelled, tz);
        }
        for minutes in &e.reminders {
            out += &alarm(*minutes, false);
        }
        out += "END:VEVENT\r\n";
    }
    calendar(&out)
}

/// The highest revision among the parts and their number: every write takes a new, higher revision and a
/// removed part changes the number, so this changes whenever a part is added, changed or removed.
pub fn item_etag(parts: &[Event]) -> String {
    // A part's etag is its quoted revision.
    let revision = |e: &Event| e.etag.trim_matches('"').parse::<i64>().unwrap_or(0);
    let highest = parts.iter().map(revision).max().unwrap_or(0);
    format!("\"{highest}-{}\"", parts.len())
}

pub fn ical_to_events(body: &str) -> Result<ItemIn, DavError> {
    let text_of_body = unfolded(body);
    let parts = components(&text_of_body)?;
    if parts.is_empty() || !parts.iter().all(|c| is(c, "VEVENT")) {
        return Err(unsupported(
            "a calendar takes the VEVENTs of one event per item",
        ));
    }
    let plain: Vec<_> = parts
        .iter()
        .filter(|c| prop(c, "RECURRENCE-ID").is_none())
        .collect();
    let [series] = plain[..] else {
        return Err(invalid(
            "an item is one event, or one series with its changed occurrences",
        ));
    };
    if parts.len() - 1 > MAX_OVERRIDES {
        return Err(invalid(format!(
            "at most {MAX_OVERRIDES} changed occurrences"
        )));
    }
    let uid = text(series, "UID");
    if uid.is_empty() {
        return Err(invalid("the VEVENT has no UID"));
    }
    let main = event_write(series, &uid)?;
    let mut overrides: Vec<EventWrite> = Vec::new();
    for c in &parts {
        let Some(id) = prop(c, "RECURRENCE-ID") else {
            continue;
        };
        if text(c, "UID") != uid {
            return Err(invalid("the VEVENTs of an item have one UID"));
        }
        // `THISANDFUTURE` changes every later occurrence, which one stored override cannot say.
        if param(id, "RANGE").is_some() {
            return Err(invalid("RANGE in a RECURRENCE-ID is not supported"));
        }
        let mut changed = event_write(c, &uid)?;
        if changed.rrule.is_some() || !changed.exdates.is_empty() {
            return Err(invalid("a changed occurrence has neither RRULE nor EXDATE"));
        }
        let original = in_form_of_start(id, id.val.as_str(), main.tz.as_deref())?;
        if overrides
            .iter()
            .any(|o| o.original_start.as_ref() == Some(&original))
        {
            return Err(invalid("an occurrence is changed more than once"));
        }
        // An override takes its series' uid.
        changed.uid = None;
        changed.original_start = Some(original);
        overrides.push(changed);
    }
    // The service refuses these too, but only after the series has been written.
    if !overrides.is_empty() && main.rrule.is_none() {
        return Err(invalid("a changed occurrence needs a repeating event"));
    }
    if overrides.iter().any(|o| o.all_day != main.all_day) {
        return Err(invalid(
            "a changed occurrence must be all-day exactly when its series is",
        ));
    }
    Ok(ItemIn {
        uid,
        main,
        overrides,
    })
}

/// One `VEVENT` as the service takes it.
fn event_write(c: &Component, uid: &str) -> Result<EventWrite, DavError> {
    // Dropping any of these would change when the event happens.
    if props(c, "RDATE").next().is_some() || props(c, "EXRULE").next().is_some() {
        return Err(invalid("RDATE and EXRULE are not supported"));
    }
    if props(c, "RRULE").count() > 1 {
        return Err(invalid("more than one RRULE is not supported"));
    }
    let dtstart = prop(c, "DTSTART").ok_or_else(|| invalid("the VEVENT has no DTSTART"))?;
    let (start, tz) = when(dtstart)?;
    let tz_of_start = tz.as_deref();
    let end = if let Some(dtend) = prop(c, "DTEND") {
        in_form_of_start(dtend, dtend.val.as_str(), tz_of_start)?
    } else {
        // Without either an all-day event lasts its day; a timed one has no length, which the service
        // refuses.
        let end = match (prop(c, "DURATION"), start) {
            (Some(d), _) => after(start, tz_of_start, d.val.as_str().trim()),
            (None, When::Date(_)) => after(start, None, "P1D"),
            (None, When::Timed(_)) => Some(start),
        };
        end.ok_or_else(|| invalid("DURATION is not a length of time"))?
            .to_string()
    };
    let mut exdates = Vec::new();
    for p in props(c, "EXDATE") {
        for value in p.val.as_str().split(',') {
            exdates.push(in_form_of_start(p, value, tz_of_start)?);
        }
    }
    Ok(EventWrite {
        uid: Some(uid.to_owned()),
        summary: text(c, "SUMMARY"),
        description: text(c, "DESCRIPTION"),
        location: text(c, "LOCATION"),
        all_day: matches!(start, When::Date(_)),
        start: start.to_string(),
        end,
        tz,
        rrule: prop(c, "RRULE").map(|p| p.val.as_str().trim().to_owned()),
        exdates,
        reminders: reminders(c, false),
        recurring_event_id: None,
        original_start: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_forms() {
        for (text, read) in [
            ("P2W", Some((14, 0))),
            ("P3D", Some((3, 0))),
            ("PT90M", Some((0, 5400))),
            ("P1DT2H3M4S", Some((1, 7384))),
            ("PT1H30S", Some((0, 3630))),
            ("PT0S", Some((0, 0))),
            ("P1M", None),
            ("P1Y", None),
            ("PT1D", None),
            ("P1W2D", None),
            ("P1H", None),
            ("PT1M1H", None),
            ("P1DT", None),
            ("PT", None),
            ("P", None),
            ("PT5", None),
            ("P1D1D", None),
            ("PT1.5H", None),
            ("-PT1H", None),
            ("1D", None),
            ("", None),
        ] {
            assert_eq!(duration(text), read, "{text}");
        }
    }

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

    /// Each changed occurrence is a write to the service of its own.
    #[test]
    fn changed_occurrences_are_limited() {
        let item = |changed: usize| {
            let day = |i| NaiveDate::from_ymd_opt(2026, 1, 1).unwrap() + Duration::days(i as i64);
            let overrides: String = (0..changed)
                .map(|i| {
                    let d = day(i).format("%Y%m%d");
                    format!(
                        "BEGIN:VEVENT\r\nUID:s1\r\nRECURRENCE-ID:{d}T090000Z\r\n\
                         DTSTART:{d}T100000Z\r\nDTEND:{d}T110000Z\r\nEND:VEVENT\r\n"
                    )
                })
                .collect();
            ical_to_events(&format!(
                "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:s1\r\nDTSTART:20260101T090000Z\r\n\
                 DTEND:20260101T100000Z\r\nRRULE:FREQ=DAILY\r\nEND:VEVENT\r\n{overrides}END:VCALENDAR\r\n"
            ))
        };
        assert_eq!(item(500).ok().unwrap().overrides.len(), 500);
        let Err(e) = item(501) else {
            panic!("501 changed occurrences are taken");
        };
        assert_eq!(e.status(), StatusCode::FORBIDDEN);
        assert_eq!(e.precondition, Some("valid-calendar-data"));
        assert_eq!(e.message, "at most 500 changed occurrences");
    }

    #[test]
    fn control_characters_cannot_start_a_line() {
        let l = line("SUMMARY", None, "a\r\nEND:VTODO\u{0}");
        assert_eq!(l, "SUMMARY:a\\nEND:VTODO\r\n");
    }
}
