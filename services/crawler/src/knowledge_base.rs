// Hands a changed page to knowledge-base for enrichment and embedding. A failed hand-off is logged and does not fail the crawl.

use std::time::Duration;

use common::proto::knowledge_base::v1::{IngestRequest, KnowledgeBaseServiceClient};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};

const SOURCE: &str = "crawler";

pub trait KnowledgeBase: Clone + Send + Sync + 'static {
    fn ingest(
        &self,
        url: &str,
        title: &str,
        content: &str,
    ) -> impl Future<Output = Result<(), String>> + Send;
}

#[derive(Clone)]
pub struct KnowledgeBaseClient {
    inner: KnowledgeBaseServiceClient<HttpClient>,
}

impl KnowledgeBaseClient {
    pub fn new(base_url: &str, timeout: Duration) -> Result<Self, String> {
        let target = base_url
            .parse()
            .map_err(|error| format!("could not parse KNOWLEDGE_BASE_URL {base_url:?}: {error}"))?;
        Ok(Self {
            inner: KnowledgeBaseServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(timeout)
                    .proto(),
            ),
        })
    }
}

impl KnowledgeBase for KnowledgeBaseClient {
    async fn ingest(&self, url: &str, title: &str, content: &str) -> Result<(), String> {
        self.inner
            .ingest(IngestRequest {
                source: SOURCE.to_owned(),
                source_id: url.to_owned(),
                title: title.to_owned(),
                content: content.to_owned(),
                ..Default::default()
            })
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}
