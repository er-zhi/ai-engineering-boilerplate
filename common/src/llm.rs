// The public contract of every quality tier: what a caller may send, what it may ask back, and what to expect in return.

use std::collections::BTreeMap;

use crate::proto::llm_router::v1::{QualityTier, ReasoningMode, ResponseFormat};

const CHARS_PER_TOKEN: usize = 4;
const WIDE_CONTEXT_TOKENS: i32 = 131_072;
const SHORT_ANSWER_TOKENS: i32 = 8_192;
const LONG_ANSWER_TOKENS: i32 = 32_768;

pub const SERVED_TIERS: [QualityTier; 3] =
    [QualityTier::Low, QualityTier::Medium, QualityTier::High];

pub const RESPONSE_FORMATS: [ResponseFormat; 2] =
    [ResponseFormat::Text, ResponseFormat::JsonObject];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TierLimits {
    pub max_input_tokens: i32,
    pub max_output_tokens: i32,
    pub reasoning: ReasoningMode,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sampling {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: Option<i32>,
    pub stop: Vec<String>,
    pub frequency_penalty: Option<f64>,
    pub presence_penalty: Option<f64>,
    pub seed: Option<i32>,
    pub logit_bias: BTreeMap<String, f64>,
    pub logprobs: Option<bool>,
    pub top_logprobs: Option<i32>,
    pub response_format: Option<ResponseFormat>,
}

pub fn limits(tier: QualityTier) -> Option<TierLimits> {
    match tier {
        QualityTier::Unspecified => None,
        QualityTier::Low | QualityTier::Medium => Some(TierLimits {
            max_input_tokens: WIDE_CONTEXT_TOKENS,
            max_output_tokens: SHORT_ANSWER_TOKENS,
            reasoning: ReasoningMode::Off,
        }),
        QualityTier::High => Some(TierLimits {
            max_input_tokens: WIDE_CONTEXT_TOKENS,
            max_output_tokens: LONG_ANSWER_TOKENS,
            reasoning: ReasoningMode::On,
        }),
    }
}

pub fn reasoning_enabled(tier: QualityTier) -> bool {
    limits(tier).is_some_and(|limits| limits.reasoning == ReasoningMode::On)
}

pub fn estimated_tokens(system: &str, user: &str) -> i32 {
    let characters = system.chars().count() + user.chars().count();
    i32::try_from(characters.div_ceil(CHARS_PER_TOKEN)).unwrap_or(i32::MAX)
}

pub fn fit_to_limits(
    system: &str,
    user: &str,
    sampling: &mut Sampling,
    limits: TierLimits,
) -> Result<(), String> {
    let estimated_input = estimated_tokens(system, user);
    if estimated_input > limits.max_input_tokens {
        return Err(format!(
            "the prompt is about {estimated_input} tokens, over this tier's {} token input limit",
            limits.max_input_tokens
        ));
    }

    match sampling.max_tokens {
        Some(asked) if asked > limits.max_output_tokens => Err(format!(
            "max_tokens {asked} is over this tier's {} token output limit",
            limits.max_output_tokens
        )),
        Some(asked) if asked <= 0 => Err(format!("max_tokens {asked} asks for no output")),
        Some(_) => Ok(()),
        None => {
            sampling.max_tokens = Some(limits.max_output_tokens);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn low() -> TierLimits {
        limits(QualityTier::Low).unwrap()
    }

    #[test]
    fn every_served_tier_publishes_limits_and_an_unset_tier_does_not() {
        for tier in SERVED_TIERS {
            let published = limits(tier).unwrap();
            assert!(published.max_input_tokens > 0);
            assert!(published.max_output_tokens > 0);
        }

        assert_eq!(limits(QualityTier::Unspecified), None);
    }

    #[test]
    fn only_the_high_tier_reasons() {
        assert_eq!(low().reasoning, ReasoningMode::Off);
        assert_eq!(
            limits(QualityTier::Medium).unwrap().reasoning,
            ReasoningMode::Off
        );
        assert_eq!(
            limits(QualityTier::High).unwrap().reasoning,
            ReasoningMode::On
        );
    }

    #[test]
    fn a_caller_asking_for_more_output_than_the_tier_allows_is_told_the_limit() {
        let mut sampling = Sampling {
            max_tokens: Some(low().max_output_tokens + 1),
            ..Sampling::default()
        };

        let error = fit_to_limits("", "hello", &mut sampling, low()).unwrap_err();

        assert!(
            error.contains(&low().max_output_tokens.to_string()),
            "{error}"
        );
    }

    #[test]
    fn a_prompt_longer_than_the_tier_allows_is_told_the_limit() {
        let tier = TierLimits {
            max_input_tokens: 10,
            ..low()
        };
        let mut sampling = Sampling::default();

        let error = fit_to_limits("", &"x".repeat(41), &mut sampling, tier).unwrap_err();

        assert!(error.contains("10"), "{error}");
    }

    #[test]
    fn a_caller_that_names_no_limit_gets_the_tier_maximum() {
        let mut sampling = Sampling::default();

        fit_to_limits("", "hello", &mut sampling, low()).unwrap();

        assert_eq!(sampling.max_tokens, Some(low().max_output_tokens));
    }

    #[test]
    fn asking_for_no_output_at_all_is_refused() {
        let mut sampling = Sampling {
            max_tokens: Some(0),
            ..Sampling::default()
        };

        assert!(fit_to_limits("", "hello", &mut sampling, low()).is_err());
    }
}
