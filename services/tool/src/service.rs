// The tool lifecycle — create, validate, activate, list — and Execute, which runs one.

use buffa::EnumValue;
use chrono::Utc;
use common::proto::llm_router::v1::{
    CompleteRequest, LlmRouterServiceClient, QualityTier, ResponseFormat, Sampling,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, ExprTrait,
    QueryFilter,
};
use serde_json::Value;
use std::sync::LazyLock;
use std::time::Duration;
use uuid::Uuid;

use crate::entity::tool::{ActiveModel, Entity, Risk, Status};
use crate::error::ToolError;
use crate::providers::{AnySearchProvider, SearchProvider};
use crate::tools::kb_client::KnowledgeBaseClient;

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);
const FEEDBACK_PLACEHOLDER: &str = "one or two sentences";
const UNUSABLE_VERDICT: &str = "the model did not return the expected JSON shape";

static VALIDATE_SYSTEM_PROMPT: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"You validate a tool definition against 2026 API design standards. You will be given a tool's name, description, JSON Schema input_schema, JSON Schema output_schema, risk level, and timeout_seconds. Respond with exactly this JSON and nothing else, with approved false when it does not pass: {}. Approve only if input_schema and output_schema are each a plausible JSON Schema object, description clearly states what the tool does (and, for risk "write" or "destructive", what it changes), and timeout_seconds is plausible for that work."#,
        serde_json::to_string(&Verdict {
            approved: true,
            feedback: FEEDBACK_PLACEHOLDER.to_owned(),
        })
        .expect("a Verdict always serializes")
    )
});

#[derive(Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(default)]
struct Verdict {
    approved: bool,
    feedback: String,
}

impl Verdict {
    fn reached(content: &str) -> (bool, String) {
        let verdict: Self = serde_json::from_str(content).unwrap_or_default();
        let feedback = if verdict.feedback.trim().is_empty() {
            UNUSABLE_VERDICT.to_owned()
        } else {
            verdict.feedback
        };
        (verdict.approved, feedback)
    }
}

pub const MAX_SLUG_CHARS: usize = 64;
pub const MAX_NAME_CHARS: usize = 128;
pub const MAX_DESCRIPTION_CHARS: usize = 8192;
pub const MIN_TIMEOUT_SECONDS: i32 = 1;
pub const MAX_TIMEOUT_SECONDS: i32 = 300;
const DEFAULT_TOOL_HTTP_TIMEOUT: Duration = Duration::from_secs(MAX_TIMEOUT_SECONDS as u64);

pub fn bounded_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DEFAULT_TOOL_HTTP_TIMEOUT)
        .build()
        .map_err(|error| format!("could not build the outbound http client: {error}"))
}

#[derive(Debug)]
pub struct NewTool {
    slug: String,
    name: String,
    description: String,
    input_schema: Value,
    output_schema: Value,
    risk: Risk,
    timeout_seconds: i32,
}

impl NewTool {
    pub fn checked(
        slug: String,
        name: String,
        description: String,
        input_schema: Value,
        output_schema: Value,
        risk: Risk,
        timeout_seconds: i32,
    ) -> Result<Self, ToolError> {
        bounded("slug", &slug, MAX_SLUG_CHARS)?;
        bounded("name", &name, MAX_NAME_CHARS)?;
        bounded("description", &description, MAX_DESCRIPTION_CHARS)?;
        if !input_schema.is_object() || !output_schema.is_object() {
            return Err(ToolError::InvalidRequest(
                "input_schema and output_schema must be JSON objects".to_owned(),
            ));
        }
        if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&timeout_seconds) {
            return Err(ToolError::InvalidRequest(format!(
                "timeout_seconds must be between {MIN_TIMEOUT_SECONDS} and {MAX_TIMEOUT_SECONDS}, got {timeout_seconds}"
            )));
        }
        Ok(Self {
            slug,
            name,
            description,
            input_schema,
            output_schema,
            risk,
            timeout_seconds,
        })
    }
}

fn bounded(field: &str, value: &str, max_chars: usize) -> Result<(), ToolError> {
    if value.trim().is_empty() {
        return Err(ToolError::InvalidRequest(format!("{field} is required")));
    }
    let length = value.chars().count();
    if length > max_chars {
        return Err(ToolError::InvalidRequest(format!(
            "{field} is {length} characters, the maximum is {max_chars}"
        )));
    }
    Ok(())
}

pub struct Service {
    db: DatabaseConnection,
    llm: LlmRouterServiceClient<HttpClient>,
    search: AnySearchProvider,
    http: reqwest::Client,
    kb: KnowledgeBaseClient,
}

impl Service {
    pub fn new(
        db: DatabaseConnection,
        llm_router_url: &str,
        you_api_key: String,
        brave_api_key: String,
        knowledge_base_url: &str,
    ) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        let http = bounded_http_client()?;
        Ok(Self {
            db,
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(VALIDATE_TIMEOUT)
                    .proto(),
            ),
            search: AnySearchProvider::from_api_keys(you_api_key, brave_api_key, http.clone()),
            http,
            kb: KnowledgeBaseClient::new(knowledge_base_url)?,
        })
    }

    pub async fn create_tool(
        &self,
        user_id: Option<Uuid>,
        new_tool: NewTool,
    ) -> Result<i64, ToolError> {
        if claims_reserved_slug(user_id, &new_tool.slug) {
            return Err(ToolError::InvalidRequest(format!(
                "slug {:?} is reserved for a system tool",
                new_tool.slug
            )));
        }
        let now = Utc::now();
        let row = ActiveModel {
            user_id: Set(user_id),
            slug: Set(new_tool.slug),
            name: Set(new_tool.name),
            description: Set(new_tool.description),
            input_schema: Set(new_tool.input_schema),
            output_schema: Set(new_tool.output_schema),
            connection_id: Set(None),
            risk: Set(new_tool.risk),
            timeout_seconds: Set(new_tool.timeout_seconds),
            status: Set(Status::Draft),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(row.id)
    }

    pub async fn validate_tool(
        &self,
        tool_id: i64,
        user_id: Uuid,
    ) -> Result<(bool, String), ToolError> {
        let tool = self.user_owned_tool(tool_id, user_id).await?;
        if already_reviewed(tool.status) {
            return Err(ToolError::InvalidRequest(format!(
                "tool {tool_id} is not Draft"
            )));
        }
        let user_prompt = format!(
            "name: {}\ndescription: {}\ninput_schema: {}\noutput_schema: {}\nrisk: {:?}\ntimeout_seconds: {}",
            tool.name,
            tool.description,
            tool.input_schema,
            tool.output_schema,
            tool.risk,
            tool.timeout_seconds,
        );
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Medium),
                system_prompt: VALIDATE_SYSTEM_PROMPT.clone(),
                user_prompt,
                sampling: Sampling {
                    response_format: Some(EnumValue::Known(ResponseFormat::JsonObject)),
                    ..Default::default()
                }
                .into(),
                ..Default::default()
            })
            .await
            .map_err(|e| ToolError::InvalidRequest(format!("llm-router call failed: {e}")))?
            .into_owned();
        let (approved, feedback) = Verdict::reached(&response.content);

        let mut active: ActiveModel = tool.into();
        active.status = Set(if approved {
            Status::Validated
        } else {
            Status::Draft
        });
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;

        Ok((approved, feedback))
    }

    pub async fn activate_tool(&self, tool_id: i64, user_id: Uuid) -> Result<(), ToolError> {
        let tool = self.user_owned_tool(tool_id, user_id).await?;
        if tool.status != Status::Validated {
            return Err(ToolError::InvalidRequest(format!(
                "tool {tool_id} is not Validated"
            )));
        }
        let mut active: ActiveModel = tool.into();
        active.status = Set(Status::Active);
        active.updated_at = Set(Utc::now());
        active.update(&self.db).await?;
        Ok(())
    }

    pub async fn list_tools(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<Vec<crate::entity::tool::Model>, ToolError> {
        let owner_filter = user_id.map_or_else(
            || crate::entity::tool::Column::UserId.is_null(),
            |id| {
                crate::entity::tool::Column::UserId
                    .is_null()
                    .or(crate::entity::tool::Column::UserId.eq(id))
            },
        );
        Ok(Entity::find()
            .filter(owner_filter)
            .filter(crate::entity::tool::Column::Status.eq(Status::Active))
            .all(&self.db)
            .await?)
    }

    async fn user_owned_tool(
        &self,
        tool_id: i64,
        user_id: Uuid,
    ) -> Result<crate::entity::tool::Model, ToolError> {
        Entity::find_by_id(tool_id)
            .filter(crate::entity::tool::Column::UserId.eq(user_id))
            .one(&self.db)
            .await?
            .ok_or(ToolError::NotFound(tool_id))
    }

    pub async fn execute(
        &self,
        caller: Option<Uuid>,
        slug: &str,
        input_json: &str,
        idempotency_key: &str,
    ) -> Result<ExecuteOutcome, ToolError> {
        tracing::debug!(
            idempotency_key,
            slug,
            "tool execute (key logged, not enforced — no destructive side effect here yet, see the spec's Порты-equivalent note)"
        );
        let Some(tool) = self.find_tool(caller, slug).await? else {
            return Ok(ExecuteOutcome::Error(format!("no tool with slug {slug:?}")));
        };
        if crate::policy::check_policy(tool.risk) == crate::policy::PolicyDecision::RequiresApproval
        {
            return Ok(ExecuteOutcome::RequiresApproval);
        }
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| ToolError::InvalidRequest(e.to_string()))?;
        self.run_system_tool(slug, &input, registered_timeout(tool.timeout_seconds))
            .await
    }

    async fn find_tool(
        &self,
        caller: Option<Uuid>,
        slug: &str,
    ) -> Result<Option<crate::entity::tool::Model>, ToolError> {
        if let Some(user_id) = caller {
            let own = Entity::find()
                .filter(crate::entity::tool::Column::UserId.eq(user_id))
                .filter(crate::entity::tool::Column::Slug.eq(slug))
                .one(&self.db)
                .await?;
            if own.is_some() {
                return Ok(own);
            }
        }
        Ok(Entity::find()
            .filter(crate::entity::tool::Column::UserId.is_null())
            .filter(crate::entity::tool::Column::Slug.eq(slug))
            .one(&self.db)
            .await?)
    }

    async fn run_system_tool(
        &self,
        slug: &str,
        input: &Value,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        match slug {
            crate::slugs::WEB_SEARCH => match crate::args::parse::<crate::args::Search>(input) {
                Ok(args) => self.run_web_search(args).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::WEB_FETCH => match crate::args::parse::<crate::args::Fetch>(input) {
                Ok(args) => self.run_web_fetch(args, timeout).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::KB_SEARCH => match crate::args::parse::<crate::args::Search>(input) {
                Ok(args) => self.run_kb_search(args).await,
                Err(problem) => Ok(ExecuteOutcome::Error(problem)),
            },
            crate::slugs::KB_READ_DOCUMENT => {
                match crate::args::parse::<crate::args::ReadDocument>(input) {
                    Ok(args) => self.run_kb_read_document(args).await,
                    Err(problem) => Ok(ExecuteOutcome::Error(problem)),
                }
            }
            other => Ok(ExecuteOutcome::NotExecutable(format!(
                "tool {other:?} has no runnable implementation yet — user-created tools need Integrations Service"
            ))),
        }
    }

    async fn run_web_search(&self, args: crate::args::Search) -> Result<ExecuteOutcome, ToolError> {
        let limit = match args.checked_limit() {
            Ok(limit) => limit,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        Ok(match self.search.search(&args.query, limit).await {
            Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
            Err(error) => ExecuteOutcome::Error(error),
        })
    }

    async fn run_web_fetch(
        &self,
        args: crate::args::Fetch,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        let urls = match args.checked_urls() {
            Ok(urls) => urls,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        if let Err(error) = ensure_all_urls_public(&urls).await {
            return Ok(ExecuteOutcome::Error(error));
        }
        Ok(
            match crate::tools::web_fetch::fetch_richest(&self.http, &urls, timeout).await {
                Ok(page) => ExecuteOutcome::Ok(crate::tools::web_fetch::readable_page_json(&page)),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }

    async fn run_kb_search(&self, args: crate::args::Search) -> Result<ExecuteOutcome, ToolError> {
        let limit = match args.checked_limit() {
            Ok(limit) => limit,
            Err(error) => return Ok(ExecuteOutcome::Error(error)),
        };
        Ok(match self.kb.search(&args.query, limit).await {
            Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
            Err(error) => ExecuteOutcome::Error(error),
        })
    }

    async fn run_kb_read_document(
        &self,
        args: crate::args::ReadDocument,
    ) -> Result<ExecuteOutcome, ToolError> {
        let version = args.version.unwrap_or_default();
        Ok(
            match self
                .kb
                .read_document(&args.source, &args.source_id, &version)
                .await
            {
                Ok(content) => ExecuteOutcome::Ok(serde_json::json!({"content": content})),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }
}

fn claims_reserved_slug(user_id: Option<Uuid>, slug: &str) -> bool {
    user_id.is_some() && crate::slugs::RESERVED_FOR_SYSTEM_TOOLS.contains(&slug)
}

fn already_reviewed(status: Status) -> bool {
    status != Status::Draft
}

async fn ensure_all_urls_public(urls: &[String]) -> Result<(), String> {
    for url in urls {
        crate::tools::web_fetch::ensure_public_url(url).await?;
    }
    Ok(())
}

fn registered_timeout(timeout_seconds: i32) -> Duration {
    Duration::from_secs(
        timeout_seconds
            .clamp(MIN_TIMEOUT_SECONDS, MAX_TIMEOUT_SECONDS)
            .unsigned_abs()
            .into(),
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecuteOutcome {
    Ok(Value),
    RequiresApproval,
    NotExecutable(String),
    Error(String),
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::llm_router::v1::{
        CompleteResponse, DescribeTiersRequest, DescribeTiersResponse, LlmRouterService,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use serde_json::json;
    use std::sync::Arc;

    const TEST_TIMEOUT_SECONDS: i32 = 30;

    fn a_tool(slug: &str, name: &str, description: &str, risk: Risk) -> NewTool {
        NewTool::checked(
            slug.to_owned(),
            name.to_owned(),
            description.to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            risk,
            TEST_TIMEOUT_SECONDS,
        )
        .expect("a valid tool definition")
    }

    struct FakeLlmRouter {
        reply: String,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            Response::ok(CompleteResponse {
                content: self.reply.clone(),
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

    async fn serve_llm(reply: &str) -> String {
        let fake = Arc::new(FakeLlmRouter {
            reply: reply.to_owned(),
        });
        let connect = ConnectRouter::new().add_service(fake);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn service_with(llm_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(
            test.db.clone(),
            llm_url,
            "unused-in-these-tests".to_owned(),
            "unused-in-these-tests".to_owned(),
            "http://127.0.0.1:1",
        )
        .expect("service");
        (test, service)
    }

    async fn full_service_with(llm_url: &str, kb_url: &str) -> (crate::test_db::TestDb, Service) {
        let test = crate::test_db::start().await;
        let service = Service::new(
            test.db.clone(),
            llm_url,
            "unused-in-these-tests".to_owned(),
            "unused-in-these-tests".to_owned(),
            kb_url,
        )
        .expect("service");
        (test, service)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_rejects_a_user_owned_system_slug() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();

        let error = service
            .create_tool(
                Some(user_id),
                a_tool(
                    crate::slugs::WEB_SEARCH,
                    "Shadow web search",
                    "d",
                    Risk::ReadOnly,
                ),
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_starts_as_draft() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();

        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let row = crate::entity::tool::Entity::find_by_id(tool_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, Status::Draft);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_approves_and_advances_to_validated() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "clear and safe"}"#).await;
        let (test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let (approved, feedback) = service
            .validate_tool(tool_id, user_id)
            .await
            .expect("validate");

        assert!(approved);
        assert_eq!(feedback, "clear and safe");
        let row = crate::entity::tool::Entity::find_by_id(tool_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, Status::Validated);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejects_a_tool_that_is_no_longer_draft() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create");
        service
            .validate_tool(tool_id, user_id)
            .await
            .expect("first validate");

        let error = service.validate_tool(tool_id, user_id).await.unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejection_stays_draft() {
        let llm_url =
            serve_llm(r#"{"approved": false, "feedback": "description is unclear"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "does something unclear", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let (approved, _) = service
            .validate_tool(tool_id, user_id)
            .await
            .expect("validate");

        assert!(!approved);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_tool_requires_validated_status() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(
                Some(user_id),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create");

        assert!(
            service.activate_tool(tool_id, user_id).await.is_err(),
            "still Draft, not Validated"
        );

        service
            .validate_tool(tool_id, user_id)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, user_id)
            .await
            .expect("activate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_tools_hides_other_users_tools() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let owner = Uuid::new_v4();
        let stranger = Uuid::new_v4();
        create_listable_tool(&service, owner, a_tool("mine", "Mine", "d", Risk::ReadOnly)).await;

        let seen_by_stranger = service.list_tools(Some(stranger)).await.expect("list");
        let seen_by_owner = service.list_tools(Some(owner)).await.expect("list");

        assert!(seen_by_stranger.iter().all(|t| t.slug != "mine"));
        assert!(seen_by_owner.iter().any(|t| t.slug == "mine"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_read_only_system_tool_runs_it() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        seed_system_tools(&service).await;

        let outcome = service
            .execute(
                None,
                crate::slugs::KB_SEARCH,
                r#"{"query": "borrow checker"}"#,
                "e:kb_search:0",
            )
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Ok(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_argument_the_tool_does_not_declare_is_refused_naming_the_one_it_takes() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        seed_system_tools(&service).await;

        let outcome = service
            .execute(
                None,
                crate::slugs::KB_SEARCH,
                r#"{"q": "borrow checker"}"#,
                "e:kb_search:0",
            )
            .await
            .expect("execute");

        let ExecuteOutcome::Error(message) = outcome else {
            panic!("a misspelled argument must not reach the provider");
        };
        assert!(message.contains('q'), "{message}");
        assert!(message.contains("query"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_an_unregistered_slug_is_an_error() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;

        let outcome = service
            .execute(None, "nonexistent", "{}", "e:x:0")
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::Error(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_write_tool_requires_approval_without_running_it() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                a_tool("send_email", "Send Email", "Sends an email", Risk::Write),
            )
            .await
            .expect("create");

        let outcome = service
            .execute(Some(user_id), "send_email", "{}", "e:mail:0")
            .await
            .expect("execute");

        assert_eq!(outcome, ExecuteOutcome::RequiresApproval);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_user_created_tool_is_not_executable() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::test_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                a_tool("custom_thing", "Custom", "d", Risk::ReadOnly),
            )
            .await
            .expect("create");

        let outcome = service
            .execute(Some(user_id), "custom_thing", "{}", "e:c:0")
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::NotExecutable(_)));
    }

    #[test]
    fn a_name_longer_than_the_column_is_an_invalid_argument_not_a_db_error() {
        let error = NewTool::checked(
            "echo".to_owned(),
            "e".repeat(MAX_NAME_CHARS + 1),
            "Echoes input".to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            Risk::ReadOnly,
            TEST_TIMEOUT_SECONDS,
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)), "{error}");
    }

    #[test]
    fn a_slug_longer_than_the_column_is_an_invalid_argument() {
        let error = NewTool::checked(
            "s".repeat(MAX_SLUG_CHARS + 1),
            "Echo".to_owned(),
            "Echoes input".to_owned(),
            json!({"type": "object"}),
            json!({"type": "object"}),
            Risk::ReadOnly,
            TEST_TIMEOUT_SECONDS,
        )
        .unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)), "{error}");
    }

    #[test]
    fn a_timeout_outside_the_validated_range_is_rejected_at_entry() {
        for timeout_seconds in [MIN_TIMEOUT_SECONDS - 1, MAX_TIMEOUT_SECONDS + 1] {
            let error = NewTool::checked(
                "echo".to_owned(),
                "Echo".to_owned(),
                "Echoes input".to_owned(),
                json!({"type": "object"}),
                json!({"type": "object"}),
                Risk::ReadOnly,
                timeout_seconds,
            )
            .unwrap_err();

            assert!(
                matches!(error, ToolError::InvalidRequest(_)),
                "{timeout_seconds}"
            );
        }
    }

    async fn create_listable_tool(service: &Service, owner: Uuid, tool: NewTool) {
        let tool_id = service
            .create_tool(Some(owner), tool)
            .await
            .expect("create");
        service
            .validate_tool(tool_id, owner)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, owner)
            .await
            .expect("activate");
    }

    async fn seed_system_tools(service: &Service) {
        let mut kb_search = a_tool(
            crate::slugs::KB_SEARCH,
            "Knowledge base search",
            "Searches the knowledge base",
            Risk::ReadOnly,
        );
        kb_search.input_schema = crate::args::input_schema::<crate::args::Search>();
        service
            .create_tool(None, kb_search)
            .await
            .expect("seed kb_search");
    }
}
