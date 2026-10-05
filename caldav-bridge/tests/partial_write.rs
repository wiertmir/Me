mod support;

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use support::{Stack, calendar, stored};

/// The item `s1`: Mondays at 09:00 with that summary, and the occurrence of 12 October 2026 moved, with
/// its own.
fn body(summary: &str, changed: Option<&str>) -> String {
    let changed = changed.map(|summary| {
        format!(
            "BEGIN:VEVENT\r\nUID:s1\r\nRECURRENCE-ID;TZID=Europe/Warsaw:20261012T090000\r\n\
             DTSTART;TZID=Europe/Warsaw:20261012T110000\r\nDTEND;TZID=Europe/Warsaw:20261012T120000\r\n\
             SUMMARY:{summary}\r\nEND:VEVENT\r\n"
        )
    });
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\nBEGIN:VEVENT\r\nUID:s1\r\n\
         DTSTART;TZID=Europe/Warsaw:20261005T090000\r\nDTEND;TZID=Europe/Warsaw:20261005T100000\r\n\
         RRULE:FREQ=WEEKLY;COUNT=6\r\nSUMMARY:{summary}\r\nEND:VEVENT\r\n{}END:VCALENDAR\r\n",
        changed.unwrap_or_default()
    )
}

/// The `event` field of every warning logged on this thread, and the names of all their fields.
/// `#[tokio::test]` runs the bridge on the same thread. The test is alone in this file: a log line first
/// reached on another test's thread, which has no subscriber, would be switched off for this one too.
#[derive(Clone, Default)]
struct Warnings(Arc<Mutex<Vec<Warning>>>);

type Warning = (String, Vec<String>);

impl tracing::Subscriber for Warnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String, Vec<String>);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "event" {
                    self.0 = format!("{value:?}");
                }
                self.1.push(field.name().to_owned());
            }
        }
        if *event.metadata().level() == tracing::Level::WARN {
            let mut fields = Fields(String::new(), Vec::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push((fields.0, fields.1));
        }
    }
}

/// A write that fails after a part of it was stored says so in the log, with nothing of the item.
#[tokio::test]
async fn a_partial_write_is_logged() {
    let warnings = Warnings::default();
    let _guard = tracing::subscriber::set_default(warnings.clone());
    let partly = || {
        let seen = warnings.0.lock().unwrap();
        let named = seen.iter().filter(|w| w.0.contains("event_partly_written"));
        named.map(|w| w.1.clone()).collect::<Vec<_>>()
    };
    let s = Stack::spawn().await;
    let c = calendar(&s).await;
    let path = format!("{c}/s1.ics");
    let long = "x".repeat(501);

    // Refused at the first write: nothing is stored, nothing to say.
    let (st, _, _) = s.dav("PUT", &path, "alice", &[], &body(&long, None)).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert!(stored(&s, &c, "s1").await.is_empty());
    assert!(partly().is_empty());

    let item = body("Weekly", Some(&long));
    let (st, _, _) = s.dav("PUT", &path, "alice", &[], &item).await;
    assert_eq!(st, StatusCode::FORBIDDEN);
    assert_eq!(stored(&s, &c, "s1").await.len(), 1);
    assert_eq!(partly(), [["event", "user_id", "calendar_id"]]);

    // A write that succeeds says nothing either.
    let item = body("Weekly", Some("Later"));
    let (st, _, _) = s.dav("PUT", &path, "alice", &[], &item).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert_eq!(stored(&s, &c, "s1").await.len(), 2);
    assert_eq!(partly().len(), 1);
}
