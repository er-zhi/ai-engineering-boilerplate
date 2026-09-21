// Adapter for any provider speaking the OpenAI chat-completions API, OpenRouter included: one base URL, one key, OpenAI-standard settings.

use std::time::Duration;

use common::proto::llm_router::v1::{FinishReason, ResponseFormat, Sampling};
use serde_json::{Value, json};

use crate::adapters::{
    ClientFaults, KeyCheck, as_count, error_for_status, key_accepted, read_provider_response,
    unreachable_provider,
};
use crate::llm::reasoning_enabled;
use crate::provider::{CallError, Completion, Prompt, Provider};

const COMPLETIONS_PATH: &str = "/chat/completions";
const KEY_PATH: &str = "/key";

pub struct OpenAiCompatible {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    // The operator's own JSON, opaque to this service: forwarded verbatim as the request body's
    // `provider` field and never inspected beyond "is it an object" at startup. See `main`'s
    // `provider_routing` for why that is the only check made.
    provider_routing: Option<Value>,
}

impl OpenAiCompatible {
    pub fn new(
        base_url: String,
        api_key: String,
        timeout: Duration,
        provider_routing: Option<Value>,
    ) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| format!("could not build the provider client: {error}"))?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            api_key,
            provider_routing,
        })
    }

    pub async fn verify_key(&self) -> Result<(), KeyCheck> {
        let response = self
            .http
            .get(format!("{}{KEY_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| KeyCheck::Unreachable(error.to_string()))?;

        key_accepted(response.status())
    }
}

impl Provider for OpenAiCompatible {
    async fn complete(&self, model: &str, prompt: &Prompt) -> Result<Completion, CallError> {
        let response = self
            .http
            .post(format!("{}{COMPLETIONS_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&request_body(model, prompt, self.provider_routing.as_ref()))
            .send()
            .await
            .map_err(unreachable_provider)?;

        let status = response.status();
        if !status.is_success() {
            // Nothing of the caller's reaches the provider here: the prompt is this service's own.
            return Err(error_for_status(
                status.as_u16(),
                None,
                ClientFaults::AlwaysFinal,
            ));
        }

        let body = read_provider_response(response).await?;
        read_completion(&body)
    }
}

pub fn request_body(model: &str, prompt: &Prompt, provider_routing: Option<&Value>) -> Value {
    let mut messages = Vec::new();
    if !prompt.system.is_empty() {
        messages.push(json!({"role": "system", "content": prompt.system}));
    }
    messages.push(json!({"role": "user", "content": prompt.user}));

    let mut body = json!({"model": model, "messages": messages});
    apply_sampling(&mut body, &prompt.sampling);
    if !reasoning_enabled(prompt.tier) {
        body["reasoning"] = json!({"effort": "none"});
    }
    // Passed through exactly as the operator wrote it, unset leaves the body exactly as it was
    // before this field existed — see the struct doc comment above for why this stays opaque.
    if let Some(routing) = provider_routing {
        body["provider"] = routing.clone();
    }
    body
}

fn read_completion(body: &Value) -> Result<Completion, CallError> {
    let Some(choice) = body["choices"].get(0) else {
        let detail = body["error"]["message"]
            .as_str()
            .unwrap_or("the reply carried no choices");
        return Err(CallError::WorthRetrying(detail.to_owned()));
    };

    // A provider that fails partway through generation still answers 200, with the text it had got
    // to and the failure carried inside the choice. Measured: a 429 from Google returned
    // `{"tool_call":null` and `finish_reason: "error"`, and the caller showed that to the person as
    // the answer. Whatever is in `content` here was cut off mid-token, so it is not a reply.
    if let Some(detail) = choice_error(choice) {
        return Err(CallError::WorthRetrying(detail));
    }

    let content = choice["message"]["content"].as_str().unwrap_or_default();
    if content.is_empty() {
        let reasoning_tokens = as_count(
            "reasoning_tokens",
            &body["usage"]["completion_tokens_details"]["reasoning_tokens"],
        );
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

/// Why this choice is not an answer, when the provider reports a failure inside it. Both signals
/// are checked: some providers set `finish_reason` to `"error"`, some attach an `error` object, and
/// a provider may send either alone.
fn choice_error(choice: &Value) -> Option<String> {
    let reported = choice["finish_reason"].as_str().unwrap_or_default();
    let error = &choice["error"];
    if reported != "error" && !error.is_object() {
        return None;
    }
    let detail = error["message"]
        .as_str()
        .unwrap_or("the provider stopped partway through generating");
    let code = error["code"].as_i64();
    Some(match code {
        Some(code) => format!("{detail} (provider code {code})"),
        None => detail.to_owned(),
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

fn apply_sampling(body: &mut Value, sampling: &Sampling) {
    set_if_present(body, "temperature", sampling.temperature);
    set_if_present(body, "top_p", sampling.top_p);
    set_if_present(body, "max_tokens", sampling.max_tokens);
    set_if_present(body, "frequency_penalty", sampling.frequency_penalty);
    set_if_present(body, "presence_penalty", sampling.presence_penalty);
    set_if_present(body, "seed", sampling.seed);
    set_if_present(body, "logprobs", sampling.logprobs);
    set_if_present(body, "top_logprobs", sampling.top_logprobs);

    if !sampling.stop.is_empty() {
        body["stop"] = json!(sampling.stop);
    }
    if !sampling.logit_bias.is_empty() {
        body["logit_bias"] = json!(sampling.logit_bias);
    }
    let named_format = match sampling
        .response_format
        .and_then(|format| format.as_known())
    {
        Some(ResponseFormat::Text) => Some("text"),
        Some(ResponseFormat::JsonObject) => Some("json_object"),
        Some(ResponseFormat::Unspecified) | None => None,
    };
    if let Some(name) = named_format {
        body["response_format"] = json!({"type": name});
    }
}

fn set_if_present(body: &mut Value, field: &str, value: Option<impl Into<Value>>) {
    if let Some(value) = value {
        body[field] = value.into();
    }
}

fn usage_count(body: &Value, field: &str) -> i32 {
    as_count(field, &body["usage"][field])
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use buffa::EnumValue;
    use common::proto::llm_router::v1::QualityTier;

    use super::*;
    use crate::adapters::MAX_PROVIDER_RESPONSE_BYTES;
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
            None,
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
        assert_eq!(seen[0].body["reasoning"]["effort"], json!("none"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_client_fault_on_the_key_stops_the_service_from_starting() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            let stub = TestProvider::answering_key(status).await;
            let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

            assert_eq!(
                adapter.verify_key().await,
                Err(KeyCheck::Rejected),
                "{status} on the key path is a key or a base URL this deployment cannot use"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_accepts_lets_the_service_start() {
        let stub = TestProvider::answering_key(StatusCode::OK).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        adapter.verify_key().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_cannot_be_reached_at_all_does_not_block_the_start() {
        let adapter = OpenAiCompatible::new(
            "http://127.0.0.1:1".to_owned(),
            TEST_KEY.to_owned(),
            PATIENT_ENOUGH,
            None,
        )
        .unwrap();

        assert!(matches!(
            adapter.verify_key().await,
            Err(KeyCheck::Unreachable(_))
        ));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_response_over_the_byte_limit_is_rejected_before_json_parsing() {
        let stub = TestProvider::answering(
            StatusCode::OK,
            json!({"padding": "x".repeat(MAX_PROVIDER_RESPONSE_BYTES)}),
        )
        .await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        let error = adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap_err();

        let CallError::WorthRetrying(message) = error else {
            panic!("an oversized provider response should allow the backup to answer");
        };
        assert!(
            message.contains(&MAX_PROVIDER_RESPONSE_BYTES.to_string()),
            "{message}"
        );
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
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &prompt(QualityTier::Low),
            None,
        );

        assert_eq!(body["reasoning"]["effort"], json!("none"));
        assert_eq!(body["model"], json!("deepseek/deepseek-v4-flash"));
        assert_eq!(body["messages"][0]["role"], json!("system"));
        assert_eq!(body["messages"][1]["content"], json!("https://example.com"));
    }

    #[test]
    fn the_high_tier_keeps_reasoning() {
        let body = request_body("z-ai/glm-5", &prompt(QualityTier::High), None);

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
            None,
        );

        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], json!("user"));
    }

    #[test]
    fn settings_the_caller_left_unset_are_not_sent_at_all() {
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &prompt(QualityTier::Low),
            None,
        );

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
            response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
            ..Sampling::default()
        };

        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &Prompt {
                sampling,
                ..prompt(QualityTier::Low)
            },
            None,
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
    fn a_response_format_we_do_not_know_is_not_sent() {
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &Prompt {
                sampling: Sampling {
                    response_format: Some(EnumValue::Unknown(99)),
                    ..Sampling::default()
                },
                ..prompt(QualityTier::Low)
            },
            None,
        );

        assert_eq!(body.get("response_format"), None);
    }

    #[test]
    fn a_response_format_the_caller_left_unspecified_is_not_sent() {
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &Prompt {
                sampling: Sampling {
                    response_format: Some(EnumValue::Known(ResponseFormat::Unspecified)),
                    ..Sampling::default()
                },
                ..prompt(QualityTier::Low)
            },
            None,
        );

        assert_eq!(body.get("response_format"), None);
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

    /// Measured against a live 429 from Google: the HTTP reply was 200, `finish_reason` was
    /// `"error"`, and `content` held `{"tool_call":null` — half a JSON object, which the caller
    /// could not parse and showed to the person as the answer.
    #[test]
    fn a_choice_that_reports_an_error_is_not_an_answer_however_much_text_it_carries() {
        let body = json!({
            "choices": [{
                "message": {"content": "{\"tool_call\":null"},
                "finish_reason": "error",
                "error": {"code": 429, "message": "rate limit"},
            }],
        });

        let error = read_completion(&body).expect_err("a cut-off generation is not a completion");

        let CallError::WorthRetrying(detail) = error else {
            panic!("a provider that gave up partway is worth asking again: {error:?}");
        };
        assert!(
            detail.contains("rate limit") && detail.contains("429"),
            "{detail}"
        );
    }

    /// Some providers attach the error without touching `finish_reason`.
    #[test]
    fn a_choice_carrying_an_error_object_alone_is_still_not_an_answer() {
        let body = json!({
            "choices": [{
                "message": {"content": "partial"},
                "error": {"message": "upstream closed the connection"},
            }],
        });

        assert!(matches!(
            read_completion(&body),
            Err(CallError::WorthRetrying(_))
        ));
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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rate_limit_on_the_key_probe_does_not_stop_the_service_from_starting() {
        let stub = TestProvider::answering_key(StatusCode::TOO_MANY_REQUESTS).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        assert!(
            matches!(adapter.verify_key().await, Err(KeyCheck::Unreachable(_))),
            "a rate limiter must not crash-loop the container"
        );
    }

    // The operator's routing policy is opaque JSON to this service: no field name of it is
    // spelled out here, on purpose. These fixtures use a shape no real policy would, so the
    // tests prove pass-through rather than encoding any provider's actual routing vocabulary.
    fn opaque_operator_policy() -> Value {
        json!({"an_operator_chosen_field": "an_operator_chosen_value", "nested": {"k": 1}})
    }

    #[test]
    fn without_a_routing_policy_the_body_carries_no_provider_field() {
        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &prompt(QualityTier::Low),
            None,
        );

        assert_eq!(body.get("provider"), None);
    }

    #[test]
    fn a_configured_routing_policy_is_forwarded_as_the_provider_field_verbatim() {
        let policy = opaque_operator_policy();

        let body = request_body(
            "deepseek/deepseek-v4-flash",
            &prompt(QualityTier::Low),
            Some(&policy),
        );

        assert_eq!(body["provider"], policy);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn without_a_routing_policy_the_wire_body_is_unchanged_from_today() {
        let stub = TestProvider::answering(StatusCode::OK, answered_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH).await;

        adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap();

        assert_eq!(stub.seen()[0].body.get("provider"), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_configured_routing_policy_reaches_the_provider_byte_for_byte() {
        let stub = TestProvider::answering(StatusCode::OK, answered_reply()).await;
        let policy = opaque_operator_policy();
        let adapter = OpenAiCompatible::new(
            format!("{}/", stub.base_url()),
            TEST_KEY.to_owned(),
            PATIENT_ENOUGH,
            Some(policy.clone()),
        )
        .unwrap();

        adapter
            .complete("deepseek/deepseek-v4-flash", &prompt(QualityTier::Low))
            .await
            .unwrap();

        assert_eq!(stub.seen()[0].body["provider"], policy);
    }
}
