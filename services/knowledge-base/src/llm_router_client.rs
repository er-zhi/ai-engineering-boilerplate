// Talks to llm-router over gRPC for enrichment (Complete, low tier, JSON object); embeds via the native embedder — see embedder_client.rs.

use std::sync::LazyLock;
use std::time::Duration;

use buffa::EnumValue;
use common::proto::llm_router::v1::{
    CompleteRequest, LlmRouterServiceClient, QualityTier, ResponseFormat, Sampling,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use serde_json::Value;

use crate::embedder_client::EmbedderClient;
use crate::entity::document::PageType;
use crate::llm_client::{EmbedKind, Embedded, Enrichment, LlmClient};
use crate::search;

const MAX_KEYWORDS: usize = 32;
const MAX_KEYWORD_CHARS: usize = 64;
const MAX_SUMMARY_CHARS: usize = 2_000;
const MAX_ENRICHMENT_CONTENT_CHARS: usize = 20_000;
static SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    let choices: Vec<String> = search::classifiable_page_types()
        .into_iter()
        .map(|declared| format!("{:?}", search::page_type_word(declared)))
        .collect();
    format!(
        "Classify the page and reply with only a JSON object: \
{{\"page_type\": one of {}, \
\"keywords\": a short list of free-form terms, \"summary\": a one or two sentence summary}}. \
No other text.",
        choices.join(", ")
    )
});

pub struct LlmRouterClient {
    inner: LlmRouterServiceClient<HttpClient>,
    embedder: EmbedderClient,
}

impl LlmRouterClient {
    pub fn new(
        llm_router_url: &str,
        timeout: Duration,
        embedder_url: &str,
        embedder_timeout: Duration,
    ) -> Result<Self, String> {
        let target = llm_router_url.parse().map_err(|error| {
            format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {error}")
        })?;
        Ok(Self {
            inner: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(timeout)
                    .proto(),
            ),
            embedder: EmbedderClient::new(embedder_url, embedder_timeout)?,
        })
    }
}

impl LlmClient for LlmRouterClient {
    async fn enrich(&self, content: &str) -> Result<Enrichment, String> {
        let enrichment_content: String =
            content.chars().take(MAX_ENRICHMENT_CONTENT_CHARS).collect();
        let response = self
            .inner
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Low),
                system_prompt: SYSTEM_PROMPT.clone(),
                user_prompt: enrichment_content,
                sampling: Sampling {
                    response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .map_err(|error| error.to_string())?
            .into_owned();

        parse_enrichment(&response.content)
    }

    async fn embed(&self, content: &str, kind: EmbedKind) -> Result<Embedded, String> {
        self.embedder.embed(content, kind).await
    }
}

fn parse_enrichment(content: &str) -> Result<Enrichment, String> {
    let parsed: Value = serde_json::from_str(content)
        .map_err(|error| format!("the model's reply was not valid JSON: {error}"))?;

    let page_type = parsed["page_type"]
        .as_str()
        .and_then(search::page_type_of_word)
        .and_then(search::page_type_of)
        .unwrap_or(PageType::Other);
    let keywords = parsed["keywords"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(MAX_KEYWORDS)
        .map(|keyword| keyword.chars().take(MAX_KEYWORD_CHARS).collect())
        .collect();
    let summary = parsed["summary"]
        .as_str()
        .unwrap_or_default()
        .chars()
        .take(MAX_SUMMARY_CHARS)
        .collect();

    Ok(Enrichment {
        page_type,
        keywords,
        summary,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use common::proto::llm_router::v1::{
        CompleteResponse, DescribeTiersRequest, DescribeTiersResponse, LlmRouterService,
    };
    use connectrpc::{
        ConnectError, ErrorCode, RequestContext, Response, Router as ConnectRouter, ServiceRequest,
        ServiceResult,
    };

    use super::*;

    struct FakeLlmRouter {
        received: Mutex<Vec<CompleteRequest>>,
        failure: Option<ErrorCode>,
        delay: Duration,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            self.received
                .lock()
                .unwrap()
                .push(request.to_owned_message());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if let Some(code) = self.failure {
                return Err(ConnectError::new(code, "configured LLM failure"));
            }
            Response::ok(CompleteResponse {
                content:
                    r#"{"page_type":"documentation","keywords":["docs"],"summary":"Summary."}"#
                        .to_owned(),
                ..Default::default()
            })
        }

        async fn describe_tiers(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeTiersRequest>,
        ) -> ServiceResult<DescribeTiersResponse> {
            Response::ok(DescribeTiersResponse::default())
        }
    }

    async fn start_fake_llm_router(fake: Arc<FakeLlmRouter>) -> String {
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    async fn client_with(fake: Arc<FakeLlmRouter>, timeout: Duration) -> LlmRouterClient {
        let url = start_fake_llm_router(fake).await;
        LlmRouterClient::new(&url, timeout, "http://127.0.0.1:1", Duration::from_secs(1)).unwrap()
    }

    #[tokio::test]
    async fn enrichment_sends_low_tier_json_request_and_truncates_the_prompt() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            failure: None,
            delay: Duration::ZERO,
        });
        let client = client_with(Arc::clone(&fake), Duration::from_secs(1)).await;
        let content = "é".repeat(MAX_ENRICHMENT_CONTENT_CHARS + 1);

        let enrichment = client.enrich(&content).await.unwrap();

        assert_eq!(enrichment.page_type, PageType::Documentation);
        let received = fake.received.lock().unwrap();
        let request = &received[0];
        assert_eq!(request.tier, EnumValue::Known(QualityTier::Low));
        assert_eq!(request.system_prompt, *SYSTEM_PROMPT);
        assert_eq!(
            request.user_prompt.chars().count(),
            MAX_ENRICHMENT_CONTENT_CHARS
        );
        assert_eq!(
            request.user_prompt,
            "é".repeat(MAX_ENRICHMENT_CONTENT_CHARS)
        );
        assert_eq!(
            request.sampling.response_format,
            Some(EnumValue::Known(ResponseFormat::JsonObject))
        );
    }

    #[tokio::test]
    async fn enrichment_surfaces_an_upstream_rpc_error() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            failure: Some(ErrorCode::ResourceExhausted),
            delay: Duration::ZERO,
        });
        let client = client_with(fake, Duration::from_secs(1)).await;

        let error = client.enrich("content").await.unwrap_err();

        assert!(error.contains("resource_exhausted"), "{error}");
    }

    #[tokio::test]
    async fn enrichment_honours_the_configured_timeout() {
        let fake = Arc::new(FakeLlmRouter {
            received: Mutex::new(Vec::new()),
            failure: None,
            delay: Duration::from_millis(100),
        });
        let client = client_with(fake, Duration::from_millis(10)).await;

        let error = client.enrich("content").await.unwrap_err();

        assert!(error.contains("deadline_exceeded"), "{error}");
    }

    #[test]
    fn a_well_formed_reply_is_parsed_in_full() {
        let enrichment = parse_enrichment(
            r#"{"page_type": "product", "keywords": ["wireless headphones", "noise cancelling"], "summary": "Product page for headphones."}"#,
        )
        .unwrap();

        assert_eq!(enrichment.page_type, PageType::Product);
        assert_eq!(
            enrichment.keywords,
            ["wireless headphones", "noise cancelling"]
        );
        assert_eq!(enrichment.summary, "Product page for headphones.");
    }

    #[test]
    fn a_page_type_we_do_not_know_falls_back_to_other() {
        let enrichment =
            parse_enrichment(r#"{"page_type": "faq", "keywords": [], "summary": ""}"#).unwrap();

        assert_eq!(enrichment.page_type, PageType::Other);
    }

    #[test]
    fn keywords_past_the_cap_are_dropped_not_the_whole_reply() {
        let many = (0..MAX_KEYWORDS + 5)
            .map(|n| format!("\"k{n}\""))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"page_type": "other", "keywords": [{many}], "summary": ""}}"#);

        let enrichment = parse_enrichment(&body).unwrap();

        assert_eq!(enrichment.keywords.len(), MAX_KEYWORDS);
    }

    #[test]
    fn an_overlong_keyword_is_truncated_not_dropped() {
        let long_keyword = "k".repeat(MAX_KEYWORD_CHARS + 20);
        let body =
            format!(r#"{{"page_type": "other", "keywords": ["{long_keyword}"], "summary": ""}}"#);

        let enrichment = parse_enrichment(&body).unwrap();

        assert_eq!(enrichment.keywords[0].len(), MAX_KEYWORD_CHARS);
    }

    #[test]
    fn an_overlong_summary_is_truncated_not_dropped() {
        let long_summary = "s".repeat(MAX_SUMMARY_CHARS + 20);
        let body =
            format!(r#"{{"page_type": "other", "keywords": [], "summary": "{long_summary}"}}"#);

        let enrichment = parse_enrichment(&body).unwrap();

        assert_eq!(enrichment.summary.chars().count(), MAX_SUMMARY_CHARS);
    }

    #[test]
    fn text_that_is_not_json_is_reported_rather_than_panicking() {
        let error = parse_enrichment("not json").unwrap_err();

        assert!(error.contains("JSON"), "{error}");
    }

    #[test]
    fn missing_fields_default_rather_than_fail() {
        let enrichment = parse_enrichment("{}").unwrap();

        assert_eq!(enrichment.page_type, PageType::Other);
        assert!(enrichment.keywords.is_empty());
        assert_eq!(enrichment.summary, "");
    }
}
