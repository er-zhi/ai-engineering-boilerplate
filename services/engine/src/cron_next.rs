// The one place engine talks to the `cron` crate: turning a stored cron expression plus "now"
// into the next fire time. Kept apart from entity/schedule.rs (the table) and scheduler.rs (the
// loop) so both can share it, and so the crate's error type never leaks past this function.
//
// Field convention is the `cron` crate's own, not classic 5-field crontab: six fields
// (sec min hour day-of-month month day-of-week) with an optional seventh (year). "0 0 * * * *" is
// therefore hourly on the hour, and a bare 5-field "* * * * *" is rejected.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;

/// The first time `cron_expr` fires strictly after `after`, or a human-readable reason it can't
/// be computed (a malformed expression, or one whose only matches are already in the past — a
/// pinned year, say).
pub fn next_fire_after(cron_expr: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let schedule = Schedule::from_str(cron_expr)
        .map_err(|error| format!("invalid cron expression `{cron_expr}`: {error}"))?;
    schedule
        .after(&after)
        .next()
        .ok_or_else(|| format!("cron expression `{cron_expr}` never fires again after {after}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn an_hourly_expression_fires_on_the_next_hour() {
        assert_eq!(
            next_fire_after("0 0 * * * *", at("2026-09-15T10:30:00Z")).expect("valid"),
            at("2026-09-15T11:00:00Z")
        );
    }

    #[test]
    fn the_next_fire_time_is_strictly_after_the_given_instant() {
        // Standing exactly on a fire time must advance, not return the same instant — otherwise
        // the scheduler would re-claim the same schedule forever.
        assert_eq!(
            next_fire_after("0 0 * * * *", at("2026-09-15T11:00:00Z")).expect("valid"),
            at("2026-09-15T12:00:00Z")
        );
    }

    #[test]
    fn a_malformed_expression_is_rejected() {
        assert!(next_fire_after("not a cron expression", Utc::now()).is_err());
        // Five fields is classic crontab, not this crate's format — rejected rather than
        // silently reinterpreted.
        assert!(next_fire_after("* * * * *", Utc::now()).is_err());
    }
}
