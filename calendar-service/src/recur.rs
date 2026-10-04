use std::str::FromStr;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, NaiveTime, Utc};
use chrono_tz::Tz;
use common::{ApiError, ApiResult};
use rrule::{Frequency, RRule, RRuleSet};

use crate::time::When;

// The crate's own cap on a single query result.
const CRATE_MAX: usize = u16::MAX as usize;

fn invalid(message: String) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

/// The series start as the crate wants it: the wall-clock reading on UTC, so the crate does no zone
/// arithmetic of its own. Results are read back as wall-clock `When`s and turned into instants by
/// `time::to_utc`, which keeps skipped and repeated local times in the series.
fn dt_start(start: When) -> DateTime<rrule::Tz> {
    let naive = match start {
        When::Date(d) => d.and_time(NaiveTime::MIN),
        When::Timed(t) => t,
    };
    naive.and_utc().with_timezone(&rrule::Tz::UTC)
}

fn build(rule: &str, start: When, tz: Tz) -> ApiResult<RRuleSet> {
    let bad = |e: rrule::RRuleError| invalid(format!("invalid rrule: {e}"));
    // The crate accepts any `NAME:` prefix and extra lines (RRULE:, EXRULE:, RDATE:); a bare rule has neither.
    if rule.contains([':', '\n', '\r']) {
        return Err(invalid(
            "rrule must be a bare rule such as FREQ=WEEKLY".into(),
        ));
    }
    let mut rrule = RRule::from_str(rule).map_err(bad)?;
    if !matches!(
        rrule.get_freq(),
        Frequency::Daily | Frequency::Weekly | Frequency::Monthly | Frequency::Yearly
    ) {
        return Err(invalid(
            "rrule FREQ must be DAILY, WEEKLY, MONTHLY or YEARLY".into(),
        ));
    }
    if !(rrule.get_by_hour().is_empty()
        && rrule.get_by_minute().is_empty()
        && rrule.get_by_second().is_empty())
    {
        return Err(invalid(
            "rrule must not use BYHOUR, BYMINUTE or BYSECOND".into(),
        ));
    }
    // UNTIL with a Z is an instant: read it as wall-clock in `tz`, like everything else here.
    if let Some(until) = rrule.get_until().copied() {
        let wall = if until.timezone().is_local() {
            until.naive_local()
        } else if matches!(start, When::Timed(_)) {
            until.with_timezone(&tz).naive_local()
        } else {
            until.naive_utc()
        };
        rrule = rrule.until(wall.and_utc().with_timezone(&rrule::Tz::UTC));
    }
    rrule.build(dt_start(start)).map_err(bad)
}

fn too_many(limit: usize) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "too_many_occurrences",
        format!("more than {limit} occurrences in the requested range; narrow it"),
    )
}

/// 422 `validation` unless the rule parses for this start and its FREQ is DAILY, WEEKLY, MONTHLY or YEARLY.
pub fn validate(rrule: &str, start: When, tz: Tz) -> ApiResult<()> {
    build(rrule, start, tz).map(|_| ())
}

/// Starts of the occurrences whose [start, start + duration) overlaps [from, to), in order, `exdates` removed.
/// `duration` is the series' length on the wall clock (whole days for an all-day series): an occurrence ends
/// at its wall-clock start plus `duration`, read in `tz`, so a night across a DST change ends at the same
/// wall time as any other. `tz` is the event's zone, or the query zone for an all-day series. More than
/// `limit` results: Err 422 `too_many_occurrences`.
#[allow(clippy::too_many_arguments)]
pub fn starts(
    rrule: &str,
    start: When,
    duration: Duration,
    exdates: &[When],
    tz: Tz,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    limit: usize,
) -> ApiResult<Vec<When>> {
    // The crate works on wall-clock readings, so the window is the range read on the wall in `tz`,
    // padded for zone offsets and for `duration`; the exact overlap test follows on real instants.
    let wall = |t: DateTime<Utc>| t.with_timezone(&tz).naive_local().and_utc();
    let pad = Duration::days(2);
    let set = build(rrule, start, tz)?
        .after((wall(from) - duration - pad).with_timezone(&rrule::Tz::UTC))
        .before((wall(to) + pad).with_timezone(&rrule::Tz::UTC));
    // ponytail: the crate walks every occurrence from the series start on each query, so the cost is linear
    // in the series' age, and the caller picks that age. It is bounded by the 1900 floor on years (`time::YEARS`;
    // BYHOUR/BYMINUTE/BYSECOND are refused, so at most one occurrence per day) times the series cap per
    // calendar (`events::MAX_SERIES_PER_CALENDAR`), and the crate stops at 65535 results or 100000 candidate
    // days. If range queries get slow: jump the start forward to the last occurrence before the window (exact
    // for rules without COUNT), or cache expansions per series revision.
    let found = set.all(CRATE_MAX as u16);
    if found.limited {
        return Err(too_many(limit));
    }
    let out: Vec<When> = found
        .dates
        .into_iter()
        .map(|dt| match start {
            When::Date(_) => When::Date(dt.date_naive()),
            When::Timed(_) => When::Timed(dt.naive_utc()),
        })
        .filter(|w| !exdates.contains(w))
        // A day ends at the next midnight in `tz`, which is not always 24 hours on; nor is a night.
        .filter(|w| w.instant(tz) < to && w.end_instant(duration, tz) > from)
        .collect();
    if out.len() > limit {
        return Err(too_many(limit));
    }
    Ok(out)
}

/// Whether `at` is the start of an occurrence of the series (and not in `exdates`).
pub fn is_occurrence(rrule: &str, start: When, exdates: &[When], tz: Tz, at: When) -> bool {
    let instant = at.instant(tz);
    // A zero-length occurrence "overlaps" a window only strictly inside it, hence the second either side.
    let one = Duration::seconds(1);
    starts(
        rrule,
        start,
        Duration::zero(),
        exdates,
        tz,
        instant - one,
        instant + one,
        10,
    )
    .is_ok_and(|v| v.contains(&at))
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveDateTime};

    use super::*;

    const W: Tz = Tz::Europe__Warsaw;

    fn t(s: &str) -> When {
        When::Timed(NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap())
    }

    fn d(s: &str) -> When {
        When::Date(NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap())
    }

    fn z(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(&format!("{s}Z"))
            .unwrap()
            .to_utc()
    }

    fn weekly(exdates: &[When]) -> ApiResult<Vec<When>> {
        starts(
            "FREQ=WEEKLY",
            t("2026-10-19T09:00:00"),
            Duration::hours(1),
            exdates,
            W,
            z("2026-10-01T00:00:00"),
            z("2026-11-05T00:00:00"),
            5000,
        )
    }

    #[test]
    fn weekly_keeps_wall_clock_across_dst_change() {
        assert_eq!(
            weekly(&[]).unwrap(),
            [
                t("2026-10-19T09:00:00"),
                t("2026-10-26T09:00:00"),
                t("2026-11-02T09:00:00")
            ]
        );
    }

    #[test]
    fn exdates_are_removed() {
        let got = weekly(&[t("2026-10-26T09:00:00")]).unwrap();
        assert_eq!(got, [t("2026-10-19T09:00:00"), t("2026-11-02T09:00:00")]);
    }

    #[test]
    fn count_and_until() {
        let run = |rule| {
            starts(
                rule,
                t("2026-10-05T08:00:00"),
                Duration::hours(1),
                &[],
                W,
                z("2026-10-01T00:00:00"),
                z("2026-10-31T00:00:00"),
                5000,
            )
            .unwrap()
        };
        assert_eq!(run("FREQ=DAILY;COUNT=3").len(), 3);
        assert_eq!(
            run("FREQ=DAILY;UNTIL=20261007T060000Z"),
            [
                t("2026-10-05T08:00:00"),
                t("2026-10-06T08:00:00"),
                t("2026-10-07T08:00:00")
            ]
        );
    }

    #[test]
    fn all_day_series() {
        let got = starts(
            "FREQ=WEEKLY",
            d("2026-10-05"),
            Duration::days(1),
            &[],
            W,
            z("2026-10-01T00:00:00"),
            z("2026-10-15T00:00:00"),
            5000,
        )
        .unwrap();
        assert_eq!(got, [d("2026-10-05"), d("2026-10-12")]);
    }

    #[test]
    fn overlap_from_before_range() {
        let got = starts(
            "FREQ=DAILY",
            t("2026-10-05T08:00:00"),
            Duration::hours(3),
            &[],
            W,
            z("2026-10-07T07:00:00"), // 09:00 local; the 08:00 occurrence runs to 11:00
            z("2026-10-07T07:30:00"),
            5000,
        )
        .unwrap();
        assert_eq!(got, [t("2026-10-07T08:00:00")]);
    }

    #[test]
    fn occurrence_starting_in_a_gap_ends_after_it_starts() {
        // 02:30 to 03:00 on the wall. On 2027-03-28 02:30 does not exist and becomes 03:30 (01:30Z), so
        // the half hour runs to 02:00Z; it does not end back at 03:00 (01:00Z), before it began.
        let run = |from, to| {
            starts(
                "FREQ=DAILY",
                t("2027-03-27T02:30:00"),
                Duration::minutes(30),
                &[],
                W,
                z(from),
                z(to),
                5000,
            )
            .unwrap()
        };
        assert_eq!(
            run("2027-03-28T01:40:00", "2027-03-28T01:50:00"),
            [t("2027-03-28T02:30:00")]
        );
        assert_eq!(
            run("2027-03-28T02:00:00", "2027-03-28T02:10:00"),
            [] as [When; 0]
        );
    }

    #[test]
    fn nightly_series_keeps_its_wall_clock_end_across_dst() {
        // 22:00 to 06:00 on the wall: the night of 2027-03-27 is 7 hours long and over at 04:00Z
        let run = |from, to| {
            starts(
                "FREQ=DAILY",
                t("2027-03-25T22:00:00"),
                Duration::hours(8),
                &[],
                W,
                z(from),
                z(to),
                5000,
            )
            .unwrap()
        };
        assert_eq!(
            run("2027-03-28T04:00:00", "2027-03-28T04:30:00"),
            [] as [When; 0]
        );
        assert_eq!(
            run("2027-03-28T03:30:00", "2027-03-28T04:00:00"),
            [t("2027-03-27T22:00:00")]
        );
    }

    #[test]
    fn limit() {
        let err = starts(
            "FREQ=DAILY",
            t("2026-10-05T08:00:00"),
            Duration::hours(1),
            &[],
            W,
            z("2026-10-05T00:00:00"),
            z("2026-11-04T00:00:00"),
            10,
        )
        .unwrap_err();
        assert_eq!(err.code, "too_many_occurrences");
    }

    #[test]
    fn old_daily_series() {
        let got = starts(
            "FREQ=DAILY",
            t("2006-10-02T08:00:00"),
            Duration::hours(1),
            &[],
            W,
            z("2026-10-05T00:00:00"),
            z("2026-10-12T00:00:00"),
            5000,
        )
        .unwrap();
        assert_eq!(got.len(), 7);
    }

    #[test]
    fn monthly_on_the_31st() {
        let got = starts(
            "FREQ=MONTHLY",
            d("2026-01-31"),
            Duration::days(1),
            &[],
            Tz::UTC,
            z("2026-01-01T00:00:00"),
            z("2026-06-01T00:00:00"),
            5000,
        )
        .unwrap();
        assert_eq!(got, [d("2026-01-31"), d("2026-03-31"), d("2026-05-31")]);
    }

    #[test]
    fn validate_refuses() {
        let start = t("2026-10-05T08:00:00");
        for rule in [
            "FREQ=HOURLY",
            "FREQ=MINUTELY",
            "FREQ=SECONDLY",
            "garbage",
            "",
        ] {
            let err = validate(rule, start, W).unwrap_err();
            assert_eq!(err.code, "validation", "{rule}");
        }
        assert!(validate("FREQ=WEEKLY;BYDAY=MO,WE", start, W).is_ok());
    }

    #[test]
    fn is_occurrence_matches_series_starts() {
        let start = t("2026-10-19T09:00:00");
        assert!(is_occurrence(
            "FREQ=WEEKLY",
            start,
            &[],
            W,
            t("2026-10-26T09:00:00")
        ));
        assert!(!is_occurrence(
            "FREQ=WEEKLY",
            start,
            &[],
            W,
            t("2026-10-27T09:00:00")
        ));
        assert!(!is_occurrence(
            "FREQ=WEEKLY",
            start,
            &[t("2026-10-26T09:00:00")],
            W,
            t("2026-10-26T09:00:00")
        ));
    }

    #[test]
    fn all_day_occurrence_ends_at_the_next_midnight_in_tz() {
        let run = |from, to| {
            starts(
                "FREQ=DAILY",
                d("2027-03-25"),
                Duration::days(1),
                &[],
                W,
                z(from),
                z(to),
                5000,
            )
            .unwrap()
        };
        // 2027-03-28 lasts 23 hours in Warsaw, so it is over at 22:00Z
        assert_eq!(
            run("2027-03-28T22:00:00", "2027-03-29T22:00:00"),
            [d("2027-03-29")]
        );
        let got = starts(
            "FREQ=DAILY",
            d("2026-10-20"),
            Duration::days(1),
            &[],
            W,
            z("2026-10-25T22:30:00"),
            z("2026-10-25T22:45:00"),
            5000,
        )
        .unwrap();
        assert_eq!(got, [d("2026-10-25")]);
    }

    fn starts_in(rule: &str, start: When, tz: Tz, from: &str, to: &str) -> Vec<When> {
        starts(
            rule,
            start,
            Duration::hours(1),
            &[],
            tz,
            z(from),
            z(to),
            5000,
        )
        .unwrap()
    }

    #[test]
    fn weekly_from_a_gap_start_keeps_wall_clock() {
        let start = t("2027-03-28T02:30:00");
        let got = starts_in(
            "FREQ=WEEKLY",
            start,
            W,
            "2027-03-27T00:00:00",
            "2027-04-12T00:00:00",
        );
        assert_eq!(
            got,
            [start, t("2027-04-04T02:30:00"), t("2027-04-11T02:30:00")]
        );
        assert!(is_occurrence("FREQ=WEEKLY", start, &[], W, start));
    }

    #[test]
    fn gap_day_occurrence_keeps_identity() {
        let start = t("2027-03-26T02:30:00");
        let gap = t("2027-03-28T02:30:00");
        let got = starts_in(
            "FREQ=DAILY",
            start,
            W,
            "2027-03-26T00:00:00",
            "2027-03-30T00:00:00",
        );
        assert!(got.contains(&gap), "{got:?}");
        assert!(is_occurrence("FREQ=DAILY", start, &[], W, gap));
        let got = starts(
            "FREQ=DAILY",
            start,
            Duration::hours(1),
            &[gap],
            W,
            z("2027-03-26T00:00:00"),
            z("2027-03-30T00:00:00"),
            5000,
        )
        .unwrap();
        assert!(!got.contains(&gap));
    }

    #[test]
    fn midnight_transition_zones_keep_every_day() {
        let havana = Tz::America__Havana;
        let got = starts_in(
            "FREQ=DAILY",
            t("2026-03-06T00:30:00"),
            havana,
            "2026-03-06T00:00:00",
            "2026-03-11T00:00:00",
        );
        assert!(got.contains(&t("2026-03-08T00:30:00")), "{got:?}");
        let got = starts_in(
            "FREQ=DAILY",
            t("2026-10-30T00:30:00"),
            havana,
            "2026-10-30T00:00:00",
            "2026-11-04T00:00:00",
        );
        assert!(got.contains(&t("2026-11-01T00:30:00")), "{got:?}");
    }

    #[test]
    fn sub_daily_by_parts_are_refused() {
        for rule in [
            "FREQ=DAILY;BYHOUR=1,2",
            "FREQ=DAILY;BYMINUTE=0,30",
            "FREQ=DAILY;BYSECOND=0,30",
        ] {
            let err = validate(rule, t("2026-10-05T08:00:00"), W).unwrap_err();
            assert_eq!(err.code, "validation", "{rule}");
        }
    }

    #[test]
    fn prefixes_and_line_breaks_are_refused() {
        for rule in [
            "RRULE:FREQ=DAILY",
            "EXRULE:FREQ=DAILY",
            "FREQ=DAILY\nEXRULE:FREQ=DAILY",
            "FREQ=DAILY\r\nRDATE:20261005T080000Z",
        ] {
            let err = validate(rule, t("2026-10-05T08:00:00"), W).unwrap_err();
            assert_eq!(err.code, "validation", "{rule:?}");
        }
    }
}
