// Adapter for any provider speaking the OpenAI chat-completions API, OpenRouter included: one base URL, one key, OpenAI-standard settings.

use std::time::Duration;

use common::llm::{Sampling, reasoning_enabled};
use common::proto::llm_router::v1::{FinishReason, ResponseFormat};
use serde_json::{Value, json};

use crate::provider::{CallError, Completion, Prompt, Provider};

const COMPLETIONS_PATH: &str = "/chat/completions";
const KEY_PATH: &str = "/key";

pub struct OpenAiCompatible {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OpenAiCompatible {
    pub fn new(base_url: String, api_key: String, timeout: Duration) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| format!("could not build the provider client: {error}"))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
        })
    }

    pub async fn verify_key(&self) -> Result<(), String> {
        let response = self
            .http
            .get(format!("{}{KEY_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| format!("could not reach the provider to verify the key: {error}"))?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err("the provider rejected this API key".to_owned());
        }
        Ok(())
    }
}

impl Provider for OpenAiCompatible {
    async fn complete(&self, model: &str, prompt: &Prompt) -> Result<Completion, CallError> {
        let response = self
            .http
            .post(format!("{}{COMPLETIONS_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request_body(model, prompt))
            .send()
            .await
            .map_err(|error| {
                CallError::WorthRetrying(if error.is_timeout() {
                    "the provider did not answer in time".to_owned()
                } else {
                    format!("could not reach the provider: {error}")
                })
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(error_for_status(status.as_u16()));
        }

        let body = response.json::<Value>().await.map_err(|error| {
            CallError::WorthRetrying(format!("the provider sent a reply we cannot read: {error}"))
        })?;
        read_completion(&body)
    }
}

const TOO_MANY_REQUESTS: u16 = 429;
const FIRST_SERVER_FAULT: u16 = 500;

pub fn request_body(model: &str, prompt: &Prompt) -> Value {
    let mut messages = Vec::new();
    if !prompt.system.is_empty() {
        messages.push(json!({"role": "system", "content": prompt.system}));
    }
    messages.push(json!({"role": "user", "content": prompt.user}));

    let mut body = json!({"model": model, "messages": messages});
    apply_sampling(&mut body, &prompt.sampling);
    if !reasoning_enabled(prompt.tier) {
        body["reasoning"] = json!({"enabled": false});
    }
    body
}

pub fn read_completion(body: &Value) -> Result<Completion, CallError> {
    let Some(choice) = body["choices"].get(0) else {
        let detail = body["error"]["message"]
            .as_str()
            .unwrap_or("the reply carried no choices");
        return Err(CallError::WorthRetrying(detail.to_owned()));
    };

    let content = choice["message"]["content"].as_str().unwrap_or_default();
    if content.is_empty() {
        let reasoning_tokens = nested_count(body, "completion_tokens_details", "reasoning_tokens");
        return Err(if reasoning_tokens > 0 {
            CallError::Final(format!(
                "the model spent {reasoning_tokens} completion tokens on reasoning and returned no content"
            ))
        } else {
            CallError::WorthRetrying("the model returned no content".to_owned())
        });
    }

    Ok(Completion {
        content: content.to_owned(),
        tokens_in: usage_count(body, "prompt_tokens"),
        tokens_out: usage_count(body, "completion_tokens"),
        finish_reason: finish_reason_from(choice["finish_reason"].as_str().unwrap_or_default()),
        reply: body.clone(),
    })
}

fn finish_reason_from(reported: &str) -> FinishReason {
    match reported {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "content_filter" => FinishReason::ContentFilter,
        "tool_calls" => FinishReason::ToolCalls,
        _ => FinishReason::Unspecified,
    }
}

pub fn error_for_status(status: u16) -> CallError {
    let message = format!("the provider answered {status}");
    if status == TOO_MANY_REQUESTS || status >= FIRST_SERVER_FAULT {
        CallError::WorthRetrying(message)
    } else {
        CallError::Final(message)
    }
}

fn apply_sampling(body: &mut Value, sampling: &Sampling) {
    set(body, "temperature", sampling.temperature);
    set(body, "top_p", sampling.top_p);
    set(body, "max_tokens", sampling.max_tokens);
    set(body, "frequency_penalty", sampling.frequency_penalty);
    set(body, "presence_penalty", sampling.presence_penalty);
    set(body, "seed", sampling.seed);
    set(body, "logprobs", sampling.logprobs);
    set(body, "top_logprobs", sampling.top_logprobs);

    if !sampling.stop.is_empty() {
        body["stop"] = json!(sampling.stop);
    }
    if !sampling.logit_bias.is_empty() {
        body["logit_bias"] = json!(sampling.logit_bias);
    }
    let named_format = match sampling.response_format {
        Some(ResponseFormat::Text) => Some("text"),
        Some(ResponseFormat::JsonObject) => Some("json_object"),
        Some(ResponseFormat::Unspecified) | None => None,
    };
    if let Some(name) = named_format {
        body["response_format"] = json!({"type": name});
    }
}

fn set(body: &mut Value, field: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        body[field] = value.into();
    }
}

fn usage_count(body: &Value, field: &str) -> i32 {
    as_count(&body["usage"][field])
}

fn nested_count(body: &Value, group: &str, field: &str) -> i32 {
    as_count(&body["usage"][group][field])
}

fn as_count(value: &Value) -> i32 {
    value
        .as_i64()
        .and_then(|count| i32::try_from(count).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use common::proto::llm_router::v1::QualityTier;

    use super::*;
    use crate::test_provider::TestProvider;

    const PATIENT_ENOUGH: Duration = Duration::from_secs(5);
    const TEST_KEY: &str = "test-key";

    fn answered_reply() -> Value {
        json!({
            "choices": [{"message": {"content": "docs"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
        })
    }

    async fn adapter_for(stub: &TestProvider, timeout: Duration) -> OpenAiCompatible {
        OpenAiCompatible::new(
            format!("{}/", stub.base_url()),
            TEST_KEY.to_owned(),
            timeout,
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_carries_the_key_and_the_settings_and_returns_the_whole_reply() {
        let stub = TestProvider::answering(StatusCode::OK, answered_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        let completion = adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap();

        assert_eq!(completion.content, "docs");
        assert_eq!(completion.reply, answered_reply());

        let seen = stub.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].authorization, format!("Bearer {TEST_KEY}"));
        assert_eq!(seen[0].body["model"], json!("deepseek/deepseek-v4-flash"));
        assert_eq!(seen[0].body["reasoning"]["enabled"], json!(false));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_rejects_stops_the_service_from_starting() {
        let stub = TestProvider::answering_key(StatusCode::UNAUTHORIZED).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        let error = adapter.verify_key().await.unwrap_err();

        assert!(error.contains("key"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_accepts_lets_the_service_start() {
        let stub = TestProvider::answering_key(StatusCode::OK).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        adapter.verify_key().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_cannot_check_keys_does_not_block_the_start() {
        let stub = TestProvider::answering_key(StatusCode::NOT_FOUND).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        adapter.verify_key().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_server_fault_from_the_provider_is_worth_retrying() {
        let stub =
            TestProvider::answering(StatusCode::SERVICE_UNAVAILABLE, json!({"error": "busy"}))
                .await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        let error = adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap_err();

        assert!(matches!(error, CallError::WorthRetrying(_)), "{error:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_the_provider_rejects_is_not_retried() {
        let stub =
            TestProvider::answering(StatusCode::BAD_REQUEST, json!({"error": "bad model"})).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        let error = adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap_err();

        assert!(matches!(error, CallError::Final(_)), "{error:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_does_not_answer_in_time_is_worth_retrying() {
        let stub = TestProvider::answering_after(
            StatusCode::OK,
            answered_reply(),
            Duration::from_millis(400),
        )
        .await;
        let adapter = adapter_for(&stub, Duration::from_millis(50)).await;

        let error = adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap_err();

        let CallError::WorthRetrying(message) = error else {
            panic!("a slow provider is exactly what the backup exists for");
        };
        assert!(message.contains("in time"), "{message}");
    }

    fn prompt(tier: QualityTier) -> Prompt {
        Prompt {
            tier,
            system: "You label pages.".to_owned(),
            user: "https://example.com".to_owned(),
            sampling: Sampling::default(),
        }
    }

    #[test]
    fn cheap_tiers_turn_reasoning_off() {
        let body = request_body("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low));

        assert_eq!(body["reasoning"]["enabled"], json!(false));
        assert_eq!(body["model"], json!("deepseek/deepseek-v4-flash"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][1]["content"], json!("https://example.com"));
    }

    #[test]
    fn the_high_tier_keeps_reasoning() {
        let body = request_body("z-ai/glm-5", &prompt(QualityTier::High));

        assert_eq!(body.get("reasoning"), None);
    }

    #[test]
    fn without_a_system_prompt_only_the_user_message_is_sent() {
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &Prompt {
                system: String::new(),
                ..prompt(QualityTier::Low)
            },
        );

        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], json!("user"));
    }

    #[test]
    fn settings_the_caller_left_unset_are_not_sent_at_all() {
        let body = request_body("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low));

        for field in [
            "temperature",
            "top_p",
            "max_tokens",
            "stop",
            "frequency_penalty",
            "presence_penalty",
            "seed",
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "response_format",
        ] {
            assert_eq!(body.get(field), None, "{field} was sent without being set");
        }
    }

    #[test]
    fn every_openai_setting_the_caller_sets_is_passed_through() {
        let sampling = Sampling {
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
            response_format: Some(ResponseFormat::JsonObject),
        };

        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &Prompt {
                sampling,
                ..prompt(QualityTier::Low)
            },
        );

        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["max_tokens"], json!(300));
        assert_eq!(body["stop"], json!(["\n\n"]));
        assert_eq!(body["frequency_penalty"], json!(0.5));
        assert_eq!(body["presence_penalty"], json!(-0.5));
        assert_eq!(body["seed"], json!(42));
        assert_eq!(body["logit_bias"], json!({"1234": -100.0}));
        assert_eq!(body["logprobs"], json!(true));
        assert_eq!(body["top_logprobs"], json!(3));
        assert_eq!(body["response_format"], json!({"type": "json_object"}));
    }

    #[test]
    fn a_reply_yields_its_content_and_token_counts() {
        let body = json!({
            "choices": [{"message": {"content": "docs"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
        });

        let completion = read_completion(&body).unwrap();

        assert_eq!(completion.content, "docs");
        assert_eq!(completion.tokens_in, 10);
        assert_eq!(completion.tokens_out, 2);
        assert_eq!(completion.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn a_finish_reason_we_do_not_know_is_reported_as_unspecified() {
        let body = json!({
            "choices": [{"message": {"content": "docs"}, "finish_reason": "eos"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
        });

        let completion = read_completion(&body).unwrap();

        assert_eq!(completion.finish_reason, FinishReason::Unspecified);
    }

    #[test]
    fn a_reply_spent_entirely_on_reasoning_names_reasoning_and_is_not_retried() {
        let body = json!({
            "choices": [{"message": {"content": ""}, "finish_reason": "length"}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 300,
                "completion_tokens_details": {"reasoning_tokens": 300},
            },
        });

        let error = read_completion(&body).unwrap_err();

        let CallError::Final(message) = error else {
            panic!("a budget spent on hidden reasoning is a config fault, not a blip");
        };
        assert!(message.contains("reasoning"), "{message}");
    }

    #[test]
    fn an_empty_reply_without_reasoning_is_worth_retrying() {
        let body = json!({
            "choices": [{"message": {"content": ""}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 0},
        });

        assert!(matches!(
            read_completion(&body).unwrap_err(),
            CallError::WorthRetrying(_)
        ));
    }

    #[test]
    fn a_body_without_choices_is_worth_retrying() {
        assert!(matches!(
            read_completion(&json!({"error": {"message": "upstream hiccup"}})).unwrap_err(),
            CallError::WorthRetrying(_)
        ));
    }

    #[test]
    fn rate_limits_and_server_faults_are_worth_retrying() {
        for status in [429, 500, 502, 503] {
            assert!(
                matches!(error_for_status(status), CallError::WorthRetrying(_)),
                "status {status} should reach the backup"
            );
        }
    }

    #[test]
    fn a_rejected_request_is_not_retried() {
        for status in [400, 401, 403, 404] {
            assert!(
                matches!(error_for_status(status), CallError::Final(_)),
                "status {status} should not reach the backup"
            );
        }
    }
}
