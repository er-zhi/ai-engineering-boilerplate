// Adapter for any provider speaking the System One standard, TypeSafe AI included: one base URL, one key, one model of that class.

pub mod answers;
pub mod body;

use std::time::Duration;

use common::proto::llm_router::v1::ModelCard;
use serde_json::Value;

use crate::adapters::{
    ClientFaults, KeyCheck, error_for_status, key_accepted, read_provider_response,
    unreachable_provider,
};
use crate::decision::{Decider, Decision, NotDecided, Verdict};
use crate::provider::CallError;

use self::answers::{model_card, read_verdict};
use self::body::{audit_body, decide_body};

const DECIDE_PATH: &str = "/v1/systemone";
const MODELS_PATH: &str = "/v1/models";

pub struct SystemOne {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl SystemOne {
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

    pub async fn verify_key(&self) -> Result<(), KeyCheck> {
        let response = self
            .http
            .get(format!("{}{MODELS_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|error| KeyCheck::Unreachable(error.to_string()))?;

        key_accepted(response.status())
    }

    async fn ask(&self, model: &str, decision: &Decision) -> Result<Value, CallError> {
        let response = self
            .http
            .post(format!("{}{DECIDE_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&decide_body(model, decision))
            .send()
            .await
            .map_err(unreachable_provider)?;

        let status = response.status();
        if !status.is_success() {
            // The caller authored the state, the questions, the options and the levels, so a provider
            // rejection of the body is theirs to read; everything else a 4xx can mean is this deployment's.
            return Err(failure(
                status.as_u16(),
                response,
                ClientFaults::RefusedWhenBodyShaped,
            )
            .await);
        }
        read_provider_response(response).await
    }
}

impl Decider for SystemOne {
    async fn decide(&self, model: &str, decision: &Decision) -> Result<Verdict, NotDecided> {
        let sent = audit_body(model, decision);
        let decided = self
            .ask(model, decision)
            .await
            .and_then(|body| read_verdict(&body, &decision.questions, &sent));
        decided.map_err(|error| NotDecided { sent, error })
    }

    async fn models(&self) -> Result<Vec<ModelCard>, CallError> {
        let response = self
            .http
            .get(format!("{}{MODELS_PATH}", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(unreachable_provider)?;

        let status = response.status();
        if !status.is_success() {
            // A listing carries nothing of the caller's, so no status here is ever the caller's to fix.
            return Err(failure(status.as_u16(), response, ClientFaults::AlwaysFinal).await);
        }

        let body = read_provider_response(response).await?;
        let Some(models) = body["models"].as_array() else {
            return Err(CallError::Final(
                "the provider listed its models in a shape this adapter cannot read".to_owned(),
            ));
        };
        Ok(models.iter().map(model_card).collect())
    }
}

async fn failure(status: u16, response: reqwest::Response, faults: ClientFaults) -> CallError {
    let detail = read_provider_response(response)
        .await
        .ok()
        .and_then(|body| provider_message(&body));
    error_for_status(status, detail, faults)
}

// TypeSafe puts its refusals under `detail`: an object for a rejected call, a list of failures for a 422.
fn provider_message(body: &Value) -> Option<String> {
    detailed(&body["detail"]).or_else(|| {
        ["error", "message", "detail"]
            .into_iter()
            .find_map(|field| body[field].as_str())
            .map(str::to_owned)
    })
}

fn detailed(detail: &Value) -> Option<String> {
    if let Some(refusal) = detail.as_object() {
        return refusal
            .get("message")
            .or_else(|| refusal.get("error_type"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    let failures: Vec<String> = detail.as_array()?.iter().map(validation_failure).collect();
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn validation_failure(failure: &Value) -> String {
    let at = failure["loc"]
        .as_array()
        .map(|path| path.iter().map(location_step).collect::<Vec<_>>().join("."))
        .unwrap_or_default();
    let said = text(&failure["msg"]);
    match (at.is_empty(), said.is_empty()) {
        (true, _) => said,
        (false, true) => at,
        (false, false) => format!("{at}: {said}"),
    }
}

fn location_step(step: &Value) -> String {
    step.as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| step.to_string())
}

pub fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::test_system_one::{
        TestSystemOne, answered_reply, decision, department, frustration, unauthenticated_reply,
        unprocessable_reply, urgency,
    };

    const PATIENT_ENOUGH: Duration = Duration::from_secs(5);
    const TEST_KEY: &str = "test-key";
    const MODEL: &str = "jev-latest";

    fn adapter_for(stub: &TestSystemOne, timeout: Duration) -> SystemOne {
        SystemOne::new(
            format!("{}/", stub.base_url()),
            TEST_KEY.to_owned(),
            timeout,
        )
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_carries_the_key_the_state_and_every_question() {
        let stub = TestSystemOne::answering(StatusCode::OK, answered_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        let verdict = adapter
            .decide(
                MODEL,
                &decision(vec![urgency(), department(), frustration()]),
            )
            .await
            .unwrap();

        assert_eq!(verdict.model_used, "jev-1.13.0");
        assert_eq!(verdict.tokens_in, 312);
        assert_eq!(verdict.tokens_out, 48);
        assert_eq!(verdict.reply, answered_reply());

        let asked = stub.asked();
        assert_eq!(asked.len(), 1);
        assert_eq!(asked[0].authorization, format!("Bearer {TEST_KEY}"));
        let sent: Value = serde_json::from_str(&asked[0].body).unwrap();
        assert_eq!(sent["model"], json!(MODEL));
        assert_eq!(
            sent["state"],
            json!("Help! My payouts have been failing for 3 days.")
        );
        assert_eq!(sent["questions"]["is_urgent"]["type"], json!("noul"));
        assert_eq!(
            sent["questions"]["is_urgent"]["criteria"],
            json!({"true": "Explicitly time-sensitive", "false": "No urgency expressed"})
        );
        assert_eq!(sent["questions"]["department"]["type"], json!("choice"));
        assert_eq!(
            sent["questions"]["frustration"]["criteria"],
            json!(["Calm", "Frustrated", "Very angry"])
        );
        assert_eq!(
            sent["questions"]["frustration"]["instructions"],
            json!("How frustrated is the customer?")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_option_order_survives_in_the_bytes_the_provider_received() {
        let stub = TestSystemOne::answering(StatusCode::OK, answered_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        adapter
            .decide(
                MODEL,
                &decision(vec![urgency(), department(), frustration()]),
            )
            .await
            .unwrap();

        let received = &stub.asked()[0].body;
        let billing = received.find("\"billing\"").unwrap();
        let technical = received.find("\"technical\"").unwrap();
        let sales = received.find("\"sales\"").unwrap();
        assert!(billing < technical && technical < sales, "{received}");
    }

    #[test]
    fn the_validation_failures_behind_a_422_reach_the_error() {
        let error = error_for_status(
            422,
            provider_message(&unprocessable_reply()),
            ClientFaults::RefusedWhenBodyShaped,
        );

        let CallError::Refused(message) = error else {
            panic!("a validation failure is the caller's to fix, not ours to retry");
        };
        assert!(message.contains("body.questions"), "{message}");
        assert!(
            message.contains("Dictionary should have at least 1 item"),
            "{message}"
        );
    }

    #[test]
    fn the_reason_behind_a_401_reaches_the_error_without_blaming_the_caller() {
        let error = error_for_status(
            401,
            provider_message(&unauthenticated_reply()),
            ClientFaults::RefusedWhenBodyShaped,
        );

        let CallError::Final(message) = error else {
            panic!(
                "a key revoked after boot is this deployment's mistake, not the caller's request"
            );
        };
        assert!(
            message.contains("Cannot authenticate with the server"),
            "{message}"
        );
    }

    #[test]
    fn a_refusal_naming_only_its_type_still_reaches_the_error() {
        let detail = json!({"detail": {"error_type": "authentication_error"}});

        assert_eq!(
            provider_message(&detail),
            Some("authentication_error".to_owned())
        );
    }

    #[test]
    fn a_provider_that_words_its_error_plainly_is_still_understood() {
        assert_eq!(
            provider_message(&json!({"error": "questions is empty"})),
            Some("questions is empty".to_owned())
        );
        assert_eq!(provider_message(&json!({"model": "jev-latest"})), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_refuses_the_call_says_why_and_keeps_what_was_sent() {
        let stub =
            TestSystemOne::answering(StatusCode::UNPROCESSABLE_ENTITY, unprocessable_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        let failed = adapter
            .decide(MODEL, &decision(vec![urgency()]))
            .await
            .unwrap_err();

        let CallError::Refused(message) = &failed.error else {
            panic!("a 422 is the caller's own request coming back");
        };
        assert!(message.contains("body.questions"), "{message}");
        assert_eq!(failed.sent["model"], json!(MODEL));
        assert_eq!(failed.sent["questions"][0]["id"], json!("is_urgent"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_revoked_after_boot_is_never_reported_as_the_callers_request() {
        let stub =
            TestSystemOne::answering(StatusCode::UNAUTHORIZED, unauthenticated_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        let failed = adapter
            .decide(MODEL, &decision(vec![urgency()]))
            .await
            .unwrap_err();

        let CallError::Final(message) = &failed.error else {
            panic!("a 401 says nothing about the request that carried it");
        };
        assert!(message.contains("Cannot authenticate"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_that_does_not_answer_in_time_is_worth_retrying() {
        let stub = TestSystemOne::answering_after(
            StatusCode::OK,
            answered_reply(),
            Duration::from_millis(400),
        )
        .await;
        let adapter = adapter_for(&stub, Duration::from_millis(50));

        let failed = adapter
            .decide(MODEL, &decision(vec![urgency()]))
            .await
            .unwrap_err();

        let CallError::WorthRetrying(message) = &failed.error else {
            panic!("a slow provider is a transient fault");
        };
        assert!(message.contains("in time"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn any_credential_fault_on_the_key_stops_the_service_from_starting() {
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
        ] {
            let stub = TestSystemOne::listing(status, json!({})).await;
            let adapter = adapter_for(&stub, PATIENT_ENOUGH);

            assert_eq!(
                adapter.verify_key().await,
                Err(KeyCheck::Rejected),
                "{status} on the key probe is a rejection"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rate_limited_key_probe_does_not_stop_the_service_from_starting() {
        let stub = TestSystemOne::listing(StatusCode::TOO_MANY_REQUESTS, json!({})).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        assert!(
            matches!(adapter.verify_key().await, Err(KeyCheck::Unreachable(_))),
            "a rate limiter must not crash-loop the container"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_accepts_lets_the_service_start() {
        let stub = TestSystemOne::listing(StatusCode::OK, json!({"models": []})).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        adapter.verify_key().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_models_the_provider_serves_come_back_as_cards() {
        let stub = TestSystemOne::listing(
            StatusCode::OK,
            json!({"models": [{
                "name": "jev-latest",
                "description": "The current judgement model",
                "release_date": "2026-09-10T18:38:01.391457+00:00",
            }]}),
        )
        .await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        let models = adapter.models().await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "jev-latest");
        assert_eq!(models[0].description, "The current judgement model");
        assert_eq!(models[0].release_date, "2026-09-10T18:38:01.391457+00:00");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listing_this_adapter_cannot_read_is_an_error_rather_than_no_models() {
        let stub = TestSystemOne::listing(StatusCode::OK, json!({"data": []})).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        assert!(matches!(
            adapter.models().await.unwrap_err(),
            CallError::Final(_)
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_rejects_fails_the_model_listing_for_good() {
        let stub = TestSystemOne::listing(StatusCode::UNAUTHORIZED, unauthenticated_reply()).await;
        let adapter = adapter_for(&stub, PATIENT_ENOUGH);

        assert!(matches!(
            adapter.models().await.unwrap_err(),
            CallError::Final(_)
        ));
    }
}
