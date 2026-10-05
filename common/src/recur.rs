use std::str::FromStr;

use axum::http::StatusCode;
use chrono::{DateTime, NaiveTime};
use chrono_tz::Tz;
use rrule::{Frequency, RRule, RRuleSet};

use crate::{ApiError, ApiResult, time::When};

fn invalid(message: String) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

/// The series start as the crate wants it: the wall-clock reading on UTC, so the crate does no zone
/// arithmetic of its own. Results are read back as wall-clock `When`s and turned into instants by
/// `to_utc`, which keeps skipped and repeated local times in the series.
pub fn wall(start: When) -> DateTime<rrule::Tz> {
    let naive = match start {
        When::Date(d) => d.and_time(NaiveTime::MIN),
        When::Timed(t) => t,
    };
    naive.and_utc().with_timezone(&rrule::Tz::UTC)
}

pub fn build(rule: &str, start: When, tz: Tz) -> ApiResult<RRuleSet> {
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
    rrule.build(wall(start)).map_err(bad)
}

/// 422 `validation` unless the rule parses for this start and its FREQ is DAILY, WEEKLY, MONTHLY or YEARLY.
pub fn validate(rrule: &str, start: When, tz: Tz) -> ApiResult<()> {
    build(rrule, start, tz).map(|_| ())
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDateTime;

    use super::*;

    const W: Tz = Tz::Europe__Warsaw;

    fn t(s: &str) -> When {
        When::Timed(NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").unwrap())
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
