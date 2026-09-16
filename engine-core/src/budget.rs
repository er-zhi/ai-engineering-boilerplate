// Budget: how much of the execution's token/tool-call/wall-time allowance is left. Fields are
// remaining capacity, not configured limits — charge() saturates at zero rather than
// underflowing, exhausted() is the single check step() needs before starting another iteration.

use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Budget {
    pub tokens_remaining: u32,
    pub tool_calls_remaining: u32,
    #[serde(with = "duration_millis")]
    pub wall_time_remaining: Duration,
}

impl Budget {
    #[must_use]
    pub fn new(tokens: u32, tool_calls: u32, wall_time: Duration) -> Self {
        Self {
            tokens_remaining: tokens,
            tool_calls_remaining: tool_calls,
            wall_time_remaining: wall_time,
        }
    }

    pub fn charge(&mut self, tokens: u32, tool_calls: u32, wall_time: Duration) {
        self.tokens_remaining = self.tokens_remaining.saturating_sub(tokens);
        self.tool_calls_remaining = self.tool_calls_remaining.saturating_sub(tool_calls);
        self.wall_time_remaining = self.wall_time_remaining.saturating_sub(wall_time);
    }

    #[must_use]
    pub fn exhausted(&self) -> bool {
        self.tokens_remaining == 0
            || self.tool_calls_remaining == 0
            || self.wall_time_remaining.is_zero()
    }
}

mod duration_millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(u64::try_from(value.as_millis()).unwrap_or(u64::MAX))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        Ok(Duration::from_millis(u64::deserialize(d)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_saturates_at_zero() {
        let mut budget = Budget::new(10, 2, Duration::from_secs(5));
        budget.charge(15, 5, Duration::from_secs(10));
        assert_eq!(budget.tokens_remaining, 0);
        assert_eq!(budget.tool_calls_remaining, 0);
        assert_eq!(budget.wall_time_remaining, Duration::ZERO);
    }

    #[test]
    fn exhausted_when_any_field_hits_zero() {
        assert!(Budget::new(0, 5, Duration::from_secs(1)).exhausted());
        assert!(Budget::new(5, 0, Duration::from_secs(1)).exhausted());
        assert!(Budget::new(5, 5, Duration::ZERO).exhausted());
        assert!(!Budget::new(1, 1, Duration::from_millis(1)).exhausted());
    }
}
