// Translates the published proto contract into the router's own types and back.

use buffa::EnumValue;
use common::llm::{RESPONSE_FORMATS, SERVED_TIERS, Sampling, limits};
use common::proto::llm_router::v1::{
    CompleteResponse, QualityTier, ResponseFormat, Sampling as WireSampling, TierContract,
};

use crate::router::Answer;

pub fn sampling_from(wire: WireSampling) -> Sampling {
    Sampling {
        temperature: wire.temperature,
        top_p: wire.top_p,
        max_tokens: wire.max_tokens,
        stop: wire.stop,
        frequency_penalty: wire.frequency_penalty,
        presence_penalty: wire.presence_penalty,
        seed: wire.seed,
        logit_bias: wire.logit_bias.into_iter().collect(),
        logprobs: wire.logprobs,
        top_logprobs: wire.top_logprobs,
        response_format: wire
            .response_format
            .and_then(|format| format.as_known())
            .filter(|format| *format != ResponseFormat::Unspecified),
    }
}

pub fn tier_contracts() -> Vec<TierContract> {
    SERVED_TIERS
        .into_iter()
        .filter_map(|tier| {
            limits(tier).map(|published| TierContract {
                tier: EnumValue::Known(tier),
                max_input_tokens: published.max_input_tokens,
                max_output_tokens: published.max_output_tokens,
                reasoning: EnumValue::Known(published.reasoning),
                response_formats: RESPONSE_FORMATS.into_iter().map(EnumValue::Known).collect(),
                ..Default::default()
            })
        })
        .collect()
}

pub fn response_from(answer: Answer) -> CompleteResponse {
    CompleteResponse {
        content: answer.completion.content,
        model_used: answer.model_used,
        tokens_in: answer.completion.tokens_in,
        tokens_out: answer.completion.tokens_out,
        used_backup: answer.used_backup,
        finish_reason: EnumValue::Known(answer.completion.finish_reason),
        ..Default::default()
    }
}

pub fn known_tier(tier: EnumValue<QualityTier>) -> QualityTier {
    tier.as_known().unwrap_or(QualityTier::Unspecified)
}

#[cfg(test)]
mod tests {
    use common::proto::llm_router::v1::{FinishReason, ReasoningMode};

    use super::*;
    use crate::provider::Completion;

    #[test]
    fn every_setting_the_caller_sent_survives_the_translation() {
        let wire = WireSampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
            max_tokens: Some(300),
            stop: vec!["\n\n".to_owned()],
            frequency_penalty: Some(0.5),
            presence_penalty: Some(-0.5),
            seed: Some(42),
            logit_bias: [("1234".to_owned(), -100.0)].into_iter().collect(),
            logprobs: Some(true),
            top_logprobs: Some(3),
            response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
            ..Default::default()
        };

        let sampling = sampling_from(wire);

        assert_eq!(sampling.temperature, Some(0.2));
        assert_eq!(sampling.top_p, Some(0.9));
        assert_eq!(sampling.max_tokens, Some(300));
        assert_eq!(sampling.stop, ["\n\n"]);
        assert_eq!(sampling.frequency_penalty, Some(0.5));
        assert_eq!(sampling.presence_penalty, Some(-0.5));
        assert_eq!(sampling.seed, Some(42));
        assert_eq!(sampling.logit_bias.get("1234"), Some(&-100.0));
        assert_eq!(sampling.logprobs, Some(true));
        assert_eq!(sampling.top_logprobs, Some(3));
        assert_eq!(sampling.response_format, Some(ResponseFormat::JsonObject));
    }

    #[test]
    fn a_caller_that_set_nothing_asks_for_nothing() {
        let sampling = sampling_from(WireSampling::default());

        assert_eq!(sampling, Sampling::default());
    }

    #[test]
    fn a_response_format_we_do_not_know_is_left_unset() {
        let wire = WireSampling {
            response_format: Some(EnumValue::Known(ResponseFormat::Unspecified)),
            ..Default::default()
        };

        assert_eq!(sampling_from(wire).response_format, None);
    }

    #[test]
    fn the_published_contract_matches_the_limits_in_common() {
        let contracts = tier_contracts();

        assert_eq!(contracts.len(), SERVED_TIERS.len());
        for (contract, tier) in contracts.iter().zip(SERVED_TIERS) {
            let published = limits(tier).unwrap();
            assert_eq!(contract.tier, EnumValue::Known(tier));
            assert_eq!(contract.max_input_tokens, published.max_input_tokens);
            assert_eq!(contract.max_output_tokens, published.max_output_tokens);
            assert_eq!(contract.reasoning, EnumValue::Known(published.reasoning));
            assert_eq!(contract.response_formats.len(), RESPONSE_FORMATS.len());
        }
    }

    #[test]
    fn the_high_tier_is_published_as_the_reasoning_one() {
        let contracts = tier_contracts();

        let high = contracts
            .iter()
            .find(|contract| contract.tier == EnumValue::Known(QualityTier::High))
            .unwrap();

        assert_eq!(high.reasoning, EnumValue::Known(ReasoningMode::On));
    }

    #[test]
    fn an_answer_carries_the_model_and_the_counts_back_to_the_caller() {
        let answer = Answer {
            completion: Completion {
                content: "docs".to_owned(),
                tokens_in: 10,
                tokens_out: 2,
                finish_reason: FinishReason::Stop,
                reply: serde_json::json!({}),
            },
            model_used: "z-ai/glm-4.7-flash".to_owned(),
            used_backup: true,
        };

        let response = response_from(answer);

        assert_eq!(response.content, "docs");
        assert_eq!(response.model_used, "z-ai/glm-4.7-flash");
        assert_eq!(response.tokens_in, 10);
        assert_eq!(response.tokens_out, 2);
        assert!(response.used_backup);
        assert_eq!(response.finish_reason, EnumValue::Known(FinishReason::Stop));
    }

    #[test]
    fn a_tier_the_caller_did_not_set_reads_as_unspecified() {
        assert_eq!(
            known_tier(EnumValue::Known(QualityTier::Low)),
            QualityTier::Low
        );
        assert_eq!(known_tier(EnumValue::default()), QualityTier::Unspecified);
    }
}
