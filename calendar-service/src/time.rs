use std::fmt;

use axum::http::StatusCode;
use chrono::{
    DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, Offset, TimeZone, Utc,
};
use chrono_tz::Tz;
use common::{ApiError, ApiResult};

const DATE: &str = "%Y-%m-%d";
const DATE_TIME: &str = "%Y-%m-%dT%H:%M:%S";

/// An event boundary: a calendar day for all-day events, a wall-clock time otherwise.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum When {
    Date(NaiveDate),
    Timed(NaiveDateTime),
}

fn invalid(message: String) -> ApiError {
    ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "validation", message)
}

/// Exact forms only (it must print back as given): `%Y-%m-%d` when `all_day`, `%Y-%m-%dT%H:%M:%S` otherwise.
pub fn parse_when(s: &str, all_day: bool) -> ApiResult<When> {
    let parsed = if all_day {
        NaiveDate::parse_from_str(s, DATE).map(When::Date)
    } else {
        NaiveDateTime::parse_from_str(s, DATE_TIME).map(When::Timed)
    };
    // chrono also takes `9:00:00`, a leading space or `+`; only the canonical spelling is one value per instant.
    parsed.ok().filter(|w| w.to_string() == s).ok_or_else(|| {
        let form = if all_day {
            "YYYY-MM-DD"
        } else {
            "YYYY-MM-DDTHH:MM:SS"
        };
        invalid(format!("{s:?} is not a valid {form} value"))
    })
}

impl fmt::Display for When {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            When::Date(d) => write!(f, "{}", d.format(DATE)),
            When::Timed(t) => write!(f, "{}", t.format(DATE_TIME)),
        }
    }
}

/// An IANA zone name such as `Europe/Warsaw`.
pub fn parse_tz(s: &str) -> ApiResult<Tz> {
    s.parse()
        .map_err(|_| invalid(format!("{s:?} is not a known time zone")))
}

/// Wall-clock time in `tz` as an instant. A skipped time moves forward by the gap; a repeated one takes the earlier.
pub fn to_utc(local: NaiveDateTime, tz: Tz) -> DateTime<Utc> {
    match tz.from_local_datetime(&local) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => t.with_timezone(&Utc),
        // The offset in force before the gap, subtracted from `local`, lands past the gap by its length.
        // ponytail: assumes the gap is under 3 hours (true for every zone today); read the transition if one ever isn't
        LocalResult::None => {
            let before = tz
                .offset_from_local_datetime(&(local - Duration::hours(3)))
                .earliest()
                .map_or(Duration::zero(), |o| {
                    Duration::seconds(i64::from(o.fix().local_minus_utc()))
                });
            (local - before).and_utc()
        }
    }
}

impl When {
    /// Dates are midnight in `tz`.
    pub fn instant(self, tz: Tz) -> DateTime<Utc> {
        match self {
            When::Date(d) => to_utc(d.and_time(NaiveTime::MIN), tz),
            When::Timed(t) => to_utc(t, tz),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ndt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, DATE_TIME).unwrap()
    }

    fn utc(s: &str) -> DateTime<Utc> {
        ndt(s).and_utc()
    }

    const WARSAW: Tz = Tz::Europe__Warsaw;

    #[test]
    fn parse_when_accepts_exact_forms_only() {
        assert_eq!(
            parse_when("2026-10-05", true).unwrap(),
            When::Date(NaiveDate::from_ymd_opt(2026, 10, 5).unwrap())
        );
        assert_eq!(
            parse_when("2026-10-05T09:00:00", false).unwrap(),
            When::Timed(ndt("2026-10-05T09:00:00"))
        );
        for (s, all_day) in [
            ("2026-10-05T09:00:00", true),
            ("2026-10-05", false),
            ("2026-10-05T09:00:00Z", false),
            ("2026-13-01", true),
            ("2026-10-12T9:00:00", false),
            ("2026-10-12T9:0:0", false),
            (" 2026-10-12T09:00:00", false),
            ("+2026-10-12T09:00:00", false),
            ("2026-10-5", true),
        ] {
            assert!(parse_when(s, all_day).is_err(), "{s} {all_day}");
        }
    }

    #[test]
    fn when_displays_the_input_forms() {
        for (s, all_day) in [("2026-10-05", true), ("2026-10-05T09:00:00", false)] {
            assert_eq!(parse_when(s, all_day).unwrap().to_string(), s);
        }
    }

    #[test]
    fn to_utc_plain() {
        assert_eq!(
            to_utc(ndt("2026-10-19T09:00:00"), WARSAW),
            utc("2026-10-19T07:00:00")
        );
    }

    #[test]
    fn to_utc_gap() {
        assert_eq!(
            to_utc(ndt("2027-03-28T02:30:00"), WARSAW),
            utc("2027-03-28T01:30:00")
        );
    }

    #[test]
    fn to_utc_fold() {
        assert_eq!(
            to_utc(ndt("2026-10-25T02:30:00"), WARSAW),
            utc("2026-10-25T00:30:00")
        );
    }

    #[test]
    fn parse_tz_known_names_only() {
        assert_eq!(parse_tz("Europe/Warsaw").unwrap(), WARSAW);
        assert!(parse_tz("Mars/Olympus").is_err());
        assert!(parse_tz("").is_err());
    }
}
