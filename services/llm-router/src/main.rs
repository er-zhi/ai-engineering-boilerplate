// LLM calls by quality tier and structured decisions on the same state: the only service holding either provider key.

mod adapters;
mod audit;
mod budget;
mod decision;
mod decisions;
mod llm;
mod log;
mod partition;
mod provider;
mod router;
mod service;
#[cfg(test)]
mod test_db;
#[cfg(test)]
mod test_provider;
#[cfg(test)]
mod test_system_one;
mod tiers;
mod wire;

use std::sync::Arc;
use std::time::Duration;

use axum::routing::get;
use common::proto::llm_router::v1::{
    CompleteRequest, CompleteResponse, DecideRequest, DecideResponse, DescribeModelsRequest,
    DescribeModelsResponse, DescribeTiersRequest, DescribeTiersResponse, LlmRouterService,
    SystemOneService,
};
use connectrpc::{
    RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
};
use sea_orm::Database;
use serde_json::Value;

use crate::adapters::KeyCheck;
use crate::adapters::openai_compatible::OpenAiCompatible;
use crate::adapters::system_one::{self, SystemOne};
use crate::decisions::Decisions;
use crate::log::{PartitionUpkeep, PgAuditLog};
use crate::service::Router;
use crate::tiers::Tiers;
use crate::wire::tier_contracts;

// `Complete` generates prose, so it keeps the old, generous budget. `Decide` has its own, far
// shorter timeout — see `adapters::system_one::REQUEST_TIMEOUT` — deliberately not this constant,
// so a change to either one can never silently move the other.
const COMPLETION_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const PARTITION_MAINTENANCE_INTERVAL: Duration = Duration::from_secs(3600);
const DEFAULT_SYSTEM_ONE_BASE_URL: &str = "https://api.typesafe.ai";
const DEFAULT_SYSTEM_ONE_MODEL: &str = "jev-1.13.0";

struct LlmRouter {
    inner: Router<OpenAiCompatible, PgAuditLog>,
}

struct SystemOneApi {
    inner: Decisions<SystemOne, PgAuditLog>,
}

#[allow(refining_impl_trait)]
impl LlmRouterService for LlmRouter {
    async fn complete(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CompleteRequest>,
    ) -> ServiceResult<CompleteResponse> {
        let response = self.inner.complete(request.to_owned_message()).await?;
        Response::ok(response)
    }

    async fn describe_tiers(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, DescribeTiersRequest>,
    ) -> ServiceResult<DescribeTiersResponse> {
        Response::ok(DescribeTiersResponse {
            tiers: tier_contracts(),
            ..Default::default()
        })
    }
}

#[allow(refining_impl_trait)]
impl SystemOneService for SystemOneApi {
    async fn decide(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, DecideRequest>,
    ) -> ServiceResult<DecideResponse> {
        let response = self.inner.decide(request.to_owned_message()).await?;
        Response::ok(response)
    }

    async fn describe_models(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, DescribeModelsRequest>,
    ) -> ServiceResult<DescribeModelsResponse> {
        Response::ok(self.inner.describe_models().await?)
    }
}

// A variable set to blank is as good as unset: a blank model would send "model": "" on every call. What is
// set comes back trimmed — a base URL with a stray space around it would otherwise break every call, and the
// startup probe would report it as a provider we cannot reach rather than as the typo it is.
fn env(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    lookup(name)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn required(lookup: &impl Fn(&str) -> Option<String>, name: &str) -> Result<String, String> {
    env(lookup, name).ok_or_else(|| format!("{name} is not set"))
}

// The operator's own routing policy: opaque JSON, forwarded verbatim onto every completion
// request. This service checks only that it parses and is an object — never its fields — so no
// provider name or routing field name needs to live here; that vocabulary stays in `.env`, where
// the operator writes it. Unset, completion is unaffected. Malformed, or valid JSON that is not
// an object, stops the service from starting, exactly as a rejected OPENROUTER_API_KEY does — a
// routing policy silently ignored would be worse than none.
fn provider_routing(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<Value>, String> {
    const VAR: &str = "LLM_PROVIDER_ROUTING_JSON";
    let Some(raw) = env(lookup, VAR) else {
        return Ok(None);
    };
    let parsed: Value =
        serde_json::from_str(&raw).map_err(|error| format!("{VAR} is not valid JSON: {error}"))?;
    if !parsed.is_object() {
        return Err(format!("{VAR} must be a JSON object"));
    }
    Ok(Some(parsed))
}

fn from_environment(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

async fn maintain_partitions_periodically(log: impl PartitionUpkeep) {
    let mut interval = tokio::time::interval(PARTITION_MAINTENANCE_INTERVAL);
    // The first tick is immediate, and prepare_schema has just done this pass; the second is an hour away.
    interval.tick().await;
    loop {
        interval.tick().await;
        maintain_partitions(&log).await;
    }
}

async fn maintain_partitions(log: &impl PartitionUpkeep) {
    match log.maintain_partitions(chrono::Utc::now()).await {
        Ok(swept) if swept.is_empty() => {}
        Ok(swept) => tracing::info!(
            request_payloads = ?swept.request_payloads,
            decision_payloads = ?swept.decision_payloads,
            "dropped llm_router payload partitions past retention"
        ),
        Err(error) => tracing::error!("llm_router partition maintenance failed: {error}"),
    }
}

// TypeSafe AI is optional: without a key the router still serves completions, and System One says so plainly.
// A key the provider rejects is a deployment mistake, and stops startup exactly as a rejected OpenRouter key does.
async fn type_safe_ai(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<(SystemOne, String)>, String> {
    let Some(api_key) = env(&lookup, "TYPESAFE_AI_API_KEY") else {
        tracing::info!("TYPESAFE_AI_API_KEY is not set; System One is not served");
        return Ok(None);
    };
    let base_url = env(&lookup, "TYPESAFE_AI_BASE_URL")
        .unwrap_or_else(|| DEFAULT_SYSTEM_ONE_BASE_URL.to_owned());
    let model =
        env(&lookup, "TYPESAFE_AI_MODEL").unwrap_or_else(|| DEFAULT_SYSTEM_ONE_MODEL.to_owned());

    let provider = SystemOne::new(base_url, api_key, system_one::REQUEST_TIMEOUT)?;
    match provider.verify_key().await {
        Ok(()) => {}
        Err(KeyCheck::Rejected) => {
            return Err("the provider rejected TYPESAFE_AI_API_KEY".to_owned());
        }
        Err(KeyCheck::Unreachable(reason)) => {
            tracing::warn!("could not verify TYPESAFE_AI_API_KEY at startup, continuing: {reason}");
        }
    }
    Ok(Some((provider, model)))
}

async fn prepare_schema(
    db: &sea_orm::DatabaseConnection,
) -> Result<(), Box<dyn std::error::Error>> {
    partition::create_parents(db).await?;
    let plain = partition::plain_parents(db).await?;
    if !plain.is_empty() {
        tracing::error!(
            ?plain,
            "these llm_router tables exist as plain tables from before partitioning, so the \
             partitioned CREATE TABLE was a silent no-op and there is no DROP PARTITION retention \
             path. Drop them — they hold statistics and payloads, not working state — and restart."
        );
        return Err(format!("{plain:?} are not partitioned by created_at").into());
    }
    partition::maintain(db, chrono::Utc::now()).await?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    common::logging::init();
    let base_url = env(&from_environment, "OPENROUTER_BASE_URL")
        .unwrap_or_else(|| "https://openrouter.ai/api/v1".to_owned());
    let api_key = required(&from_environment, "OPENROUTER_API_KEY")?;
    let provider_routing = provider_routing(&from_environment)?;
    let provider = OpenAiCompatible::new(
        base_url,
        api_key,
        COMPLETION_REQUEST_TIMEOUT,
        provider_routing,
    )?;
    match provider.verify_key().await {
        Ok(()) => {}
        Err(KeyCheck::Rejected) => return Err("the provider rejected OPENROUTER_API_KEY".into()),
        Err(KeyCheck::Unreachable(reason)) => {
            tracing::warn!("could not verify OPENROUTER_API_KEY at startup, continuing: {reason}");
        }
    }

    let tiers = Tiers::from_vars(from_environment)?;
    let system_one = type_safe_ai(from_environment).await?;

    let database_url = required(&from_environment, "DATABASE_URL")?;
    let db = Database::connect(&database_url).await?;
    prepare_schema(&db).await?;

    let log = PgAuditLog::new(db);
    tokio::spawn(maintain_partitions_periodically(log.clone()));

    let llm_router = LlmRouter {
        inner: Router::new(provider, log.clone(), tiers),
    };
    let system_one = SystemOneApi {
        inner: Decisions::new(system_one, log),
    };

    let connect = ConnectRouter::new()
        .add_service(Arc::new(llm_router))
        .add_service(Arc::new(system_one));

    let app = axum::Router::new()
        .route("/health", get(|| async { "OK" }))
        .fallback_service(connect.into_axum_service());

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083").await?;
    tracing::info!("llm-router listening on 0.0.0.0:8083");
    axum::serve(listener, app).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::json;

    use super::*;
    use crate::test_system_one::TestSystemOne;

    async fn serving_models() -> TestSystemOne {
        TestSystemOne::listing(StatusCode::OK, json!({"models": []})).await
    }

    // A regression guard, not a behavior test: `Decide` and `Complete` must never again share one
    // timeout constant. `COMPLETION_REQUEST_TIMEOUT` stays exactly what `REQUEST_TIMEOUT` used to
    // be, and `system_one::REQUEST_TIMEOUT` (exercised by the adapter's own tests) is a different,
    // much smaller constant — see the wiring in `main` and `type_safe_ai` above.
    #[test]
    fn completion_keeps_its_old_generous_timeout_unrelated_to_decides() {
        assert_eq!(COMPLETION_REQUEST_TIMEOUT, Duration::from_secs(60));
        assert_ne!(COMPLETION_REQUEST_TIMEOUT, system_one::REQUEST_TIMEOUT);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_deployment_without_a_key_serves_no_decisions_and_still_starts() {
        assert!(type_safe_ai(|_| None).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_of_nothing_but_whitespace_counts_as_unset() {
        let blank = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some("   ".to_owned()),
            _ => None,
        };

        assert!(type_safe_ai(blank).await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_configured_deployment_serves_the_model_it_names() {
        let stub = serving_models().await;
        let configured = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some("test-key".to_owned()),
            "TYPESAFE_AI_BASE_URL" => Some(stub.base_url()),
            "TYPESAFE_AI_MODEL" => Some("jev-1.13.0".to_owned()),
            _ => None,
        };

        let (_provider, model) = type_safe_ai(configured).await.unwrap().unwrap();

        assert_eq!(model, "jev-1.13.0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_base_url_or_model_set_to_blank_falls_back_to_the_default() {
        let stub = serving_models().await;
        let blank = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some("test-key".to_owned()),
            "TYPESAFE_AI_BASE_URL" => Some(stub.base_url()),
            "TYPESAFE_AI_MODEL" => Some("  ".to_owned()),
            _ => None,
        };

        let (_provider, model) = type_safe_ai(blank).await.unwrap().unwrap();

        assert_eq!(model, DEFAULT_SYSTEM_ONE_MODEL);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_value_with_whitespace_around_it_is_trimmed_before_it_is_used() {
        let stub = serving_models().await;
        let padded = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some(" test-key ".to_owned()),
            "TYPESAFE_AI_BASE_URL" => Some(format!("  {}  ", stub.base_url())),
            "TYPESAFE_AI_MODEL" => Some("  jev-1.13.0\n".to_owned()),
            _ => None,
        };

        let (_provider, model) = type_safe_ai(padded).await.unwrap().unwrap();

        assert_eq!(model, "jev-1.13.0");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_key_the_provider_rejects_stops_the_service_from_starting() {
        let stub = TestSystemOne::listing(StatusCode::UNAUTHORIZED, json!({})).await;
        let wrong = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some("wrong-key".to_owned()),
            "TYPESAFE_AI_BASE_URL" => Some(stub.base_url()),
            _ => None,
        };

        let Err(error) = type_safe_ai(wrong).await else {
            panic!("a key the provider rejects is a deployment mistake worth failing loudly");
        };

        assert!(error.contains("TYPESAFE_AI_API_KEY"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_provider_we_cannot_reach_at_startup_does_not_stop_the_service() {
        let unreachable = |name: &str| match name {
            "TYPESAFE_AI_API_KEY" => Some("test-key".to_owned()),
            "TYPESAFE_AI_BASE_URL" => Some("http://127.0.0.1:1".to_owned()),
            _ => None,
        };

        assert!(type_safe_ai(unreachable).await.unwrap().is_some());
    }

    // These fixtures spell no provider name and no routing field name, on purpose: this service
    // never reads the policy's shape, so a test proving pass-through must not either.
    #[test]
    fn no_routing_policy_set_leaves_completion_unaffected() {
        assert_eq!(provider_routing(&|_| None).unwrap(), None);
    }

    #[test]
    fn a_routing_policy_that_is_a_json_object_is_read_through() {
        let configured = |name: &str| match name {
            "LLM_PROVIDER_ROUTING_JSON" => {
                Some(r#"{"an_operator_chosen_field":"an_operator_chosen_value"}"#.to_owned())
            }
            _ => None,
        };

        let parsed = provider_routing(&configured).unwrap().unwrap();

        assert_eq!(
            parsed,
            json!({"an_operator_chosen_field": "an_operator_chosen_value"})
        );
    }

    #[test]
    fn a_routing_policy_that_is_not_valid_json_stops_the_service_from_starting() {
        let configured = |name: &str| match name {
            "LLM_PROVIDER_ROUTING_JSON" => Some("not json".to_owned()),
            _ => None,
        };

        let Err(error) = provider_routing(&configured) else {
            panic!("malformed routing JSON is a deployment mistake worth failing loudly");
        };
        assert!(error.contains("LLM_PROVIDER_ROUTING_JSON"), "{error}");
    }

    #[test]
    fn a_routing_policy_that_is_not_a_json_object_stops_the_service_from_starting() {
        let configured = |name: &str| match name {
            "LLM_PROVIDER_ROUTING_JSON" => Some("[1,2,3]".to_owned()),
            _ => None,
        };

        let Err(error) = provider_routing(&configured) else {
            panic!(
                "a routing policy that is not an object is a deployment mistake worth failing loudly"
            );
        };
        assert!(error.contains("LLM_PROVIDER_ROUTING_JSON"), "{error}");
    }
}
