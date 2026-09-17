// Translates the published proto contract into the router's own types and back.

use buffa::EnumValue;
use common::proto::llm_router::v1::{CompleteResponse, DecideResponse, QualityTier, TierContract};
use serde::Serialize;
use serde_json::Value;

use crate::decision::Verdict;
use crate::llm::{RESPONSE_FORMATS, SERVED_TIERS, limits};
use crate::router::Answer;

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

pub fn decide_response_from(verdict: Verdict) -> DecideResponse {
    DecideResponse {
        model_used: verdict.model_used,
        answers: verdict.answers,
        tokens_in: verdict.tokens_in,
        tokens_out: verdict.tokens_out,
        ..Default::default()
    }
}

// A google.protobuf.Value is the JSON it stands for, with every number a double; a field the caller left unset is JSON null.
// A value that will not serialize is refused rather than degraded to null: the model would be asked about nothing and the call would still bill.
pub fn json_from(value: Option<impl Serialize>) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| format!("could not be read as JSON: {error}"))
}

#[cfg(test)]
mod tests {
    use common::proto::llm_router::v1::{DecideRequest, FinishReason, NoulAnswer, ReasoningMode};

    use super::*;
    use crate::provider::Completion;

    fn llm_router_answer(id: &str, noul: f64) -> common::proto::llm_router::v1::Answer {
        common::proto::llm_router::v1::Answer {
            id: id.to_owned(),
            answer: NoulAnswer {
                noul,
                ..Default::default()
            }
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_published_contract_matches_the_tier_limits() {
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
    fn a_verdict_carries_the_model_the_answers_and_the_counts_back_to_the_caller() {
        let verdict = Verdict {
            model_used: "jev-1.13.0".to_owned(),
            answers: vec![
                llm_router_answer("is_urgent", 0.92),
                llm_router_answer("is_billing", 0.11),
            ],
            tokens_in: 312,
            tokens_out: 48,
            sent: serde_json::json!({}),
            reply: serde_json::json!({}),
        };

        let response = decide_response_from(verdict);

        assert_eq!(response.model_used, "jev-1.13.0");
        assert_eq!(response.tokens_in, 312);
        assert_eq!(response.tokens_out, 48);
        let asked: Vec<&str> = response
            .answers
            .iter()
            .map(|answer| answer.id.as_str())
            .collect();
        assert_eq!(asked, ["is_urgent", "is_billing"]);
    }

    #[test]
    fn a_state_reads_as_the_json_it_stands_for_and_an_unset_one_as_null() {
        let sent: DecideRequest =
            serde_json::from_value(serde_json::json!({"state": {"payouts": "failing", "days": 3}}))
                .unwrap();

        assert_eq!(
            json_from(sent.state.into_option()).unwrap(),
            serde_json::json!({"payouts": "failing", "days": 3.0})
        );
        assert_eq!(
            json_from(DecideRequest::default().state.into_option()).unwrap(),
            Value::Null
        );
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
