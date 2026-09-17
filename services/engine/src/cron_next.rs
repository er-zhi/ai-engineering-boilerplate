// Turns a stored cron expression plus an instant into the next fire time.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use cron::Schedule;

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
        assert_eq!(
            next_fire_after("0 0 * * * *", at("2026-09-15T11:00:00Z")).expect("valid"),
            at("2026-09-15T12:00:00Z")
        );
    }

    #[test]
    fn a_malformed_expression_is_rejected() {
        assert!(next_fire_after("not a cron expression", Utc::now()).is_err());
        assert!(next_fire_after("* * * * *", Utc::now()).is_err());
    }
}
