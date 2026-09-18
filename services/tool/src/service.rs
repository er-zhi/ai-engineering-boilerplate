// The tool lifecycle — create, validate, activate, list — and Execute, which runs one.

use buffa::EnumValue;
use chrono::Utc;
use common::proto::llm_router::v1::{
    CompleteRequest, DecideRequest, LlmRouterServiceClient, Noul, QualityTier, Question,
    SystemOneServiceClient, answer::Answer as Given,
};
use connectrpc::Protocol;
use connectrpc::client::{ClientConfig, HttpClient};
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, ExprTrait,
    QueryFilter,
};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

use crate::entity::tool::{ActiveModel, Entity, Risk, Status};
use crate::error::ToolError;
use crate::providers::{AnySearchProvider, SearchProvider};
use crate::tools::kb_client::KnowledgeBaseClient;

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);

// A wrong "yes" ships an unreviewed tool definition; a wrong "no" only costs a resubmission. 0.7 leans toward
// the cheaper mistake without demanding near-certainty.
const APPROVAL_THRESHOLD: f64 = 0.7;

// A tool definition either meets the standard or it does not: that is a calibrated yes/no, not prose. Words
// are only needed to explain a refusal, so they are only paid for on a refusal.
const VALIDATION_INSTRUCTIONS: &str = "Does this tool definition meet 2026 API design standards?";
const WHEN_TRUE: &str = "input_schema and output_schema are each a plausible JSON Schema object, the description clearly states what the tool does and, for write or destructive risk, what it changes, and timeout_seconds is plausible for that work";
const WHEN_FALSE: &str = "any of those is missing, vague or implausible";

const REFUSAL_SYSTEM_PROMPT: &str = "You are told a tool definition did not meet 2026 API design standards. You will be given the tool's name, description, JSON Schema input_schema, JSON Schema output_schema, risk level, and timeout_seconds. Explain in one or two sentences why this tool definition does not meet the standard.";
const REFUSAL_FALLBACK: &str = "the review service could not explain the refusal";

pub const MAX_SLUG_CHARS: usize = 64;
pub const MAX_NAME_CHARS: usize = 128;
pub const MAX_DESCRIPTION_CHARS: usize = 8192;
pub const MIN_TIMEOUT_SECONDS: i32 = 1;
pub const MAX_TIMEOUT_SECONDS: i32 = 300;
const DEFAULT_TOOL_HTTP_TIMEOUT: Duration = Duration::from_secs(MAX_TIMEOUT_SECONDS as u64);

pub fn bounded_http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(DEFAULT_TOOL_HTTP_TIMEOUT)
        // No automatic redirects: `tools::web_fetch::get_guarded` re-checks the SSRF guard against
        // every hop itself, which only works if reqwest hands each 3xx back instead of following it.
        .redirect(reqwest::redirect::Policy::none())
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
    decider: SystemOneServiceClient<HttpClient>,
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
        let client_config = ClientConfig::new(target)
            .with_protocol(Protocol::Grpc)
            .with_default_timeout(VALIDATE_TIMEOUT)
            .proto();
        Ok(Self {
            db,
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                client_config.clone(),
            ),
            decider: SystemOneServiceClient::new(HttpClient::plaintext_http2_only(), client_config),
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

        let mut request: DecideRequest = serde_json::from_value(serde_json::json!({
            "state": definition_state(&tool),
        }))
        .map_err(|e| ToolError::InvalidRequest(format!("could not build the decision: {e}")))?;
        let mut question: Question = serde_json::from_value(serde_json::json!({
            "instructions": VALIDATION_INSTRUCTIONS,
        }))
        .map_err(|e| ToolError::InvalidRequest(format!("could not build the question: {e}")))?;
        question.id = "meets_standard".to_owned();
        question.kind = Noul {
            when_true: Some(WHEN_TRUE.to_owned()),
            when_false: Some(WHEN_FALSE.to_owned()),
            ..Default::default()
        }
        .into();
        request.questions = vec![question];

        let decided = self
            .decider
            .decide(request)
            .await
            .map_err(|e| ToolError::InvalidRequest(format!("llm-router call failed: {e}")))?
            .into_owned();
        let approved = decided
            .answers
            .iter()
            .find(|answer| answer.id == "meets_standard")
            .and_then(|answer| match answer.answer.as_ref() {
                Some(Given::Noul(noul)) => Some(noul.noul),
                _ => None,
            })
            .map(|noul| noul >= APPROVAL_THRESHOLD)
            .ok_or_else(|| {
                ToolError::InvalidRequest("the decision carried no answer".to_owned())
            })?;
        let feedback = if approved {
            String::new()
        } else {
            self.refusal_words(&tool).await
        };

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

    // A refusal is the only branch that needs prose, so Complete is only ever reached from here.
    async fn refusal_words(&self, tool: &crate::entity::tool::Model) -> String {
        let response = self
            .llm
            .complete(CompleteRequest {
                tier: EnumValue::Known(QualityTier::Medium),
                system_prompt: REFUSAL_SYSTEM_PROMPT.to_owned(),
                user_prompt: definition_text(tool),
                ..Default::default()
            })
            .await;
        match response {
            Ok(response) => {
                let content = response.into_owned().content;
                if content.trim().is_empty() {
                    REFUSAL_FALLBACK.to_owned()
                } else {
                    content
                }
            }
            Err(_) => REFUSAL_FALLBACK.to_owned(),
        }
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
        let timeout = registered_timeout(tool.timeout_seconds);
        if let Some(raw) = tool.sources.clone() {
            return self.run_declarative(&tool, &raw, &input, timeout).await;
        }
        self.run_system_tool(slug, &input, timeout).await
    }

    /// A row that declares its sources runs through one shared executor: its slug is a name, not a
    /// branch, and this service holds no knowledge of what any of them is about.
    async fn run_declarative(
        &self,
        tool: &crate::entity::tool::Model,
        raw: &Value,
        input: &Value,
        timeout: Duration,
    ) -> Result<ExecuteOutcome, ToolError> {
        let set = match crate::tools::declarative::parse_sources(raw, &tool.input_schema) {
            Ok(set) => set,
            Err(problem) => return Ok(ExecuteOutcome::NotExecutable(problem)),
        };
        Ok(
            match crate::tools::declarative::run(
                &self.http,
                &set,
                input,
                timeout,
                |url| async move { crate::tools::web_fetch::ensure_public_url(&url).await },
            )
            .await
            {
                Ok(value) => ExecuteOutcome::Ok(value),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
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

// What the decider (and, on a refusal, Complete) is told about the tool under review.
fn definition_state(tool: &crate::entity::tool::Model) -> Value {
    Value::String(definition_text(tool))
}

fn definition_text(tool: &crate::entity::tool::Model) -> String {
    format!(
        "name: {}\ndescription: {}\ninput_schema: {}\noutput_schema: {}\nrisk: {:?}\ntimeout_seconds: {}",
        tool.name,
        tool.description,
        tool.input_schema,
        tool.output_schema,
        tool.risk,
        tool.timeout_seconds,
    )
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
        Answer, CompleteResponse, DecideResponse, DescribeModelsRequest, DescribeModelsResponse,
        DescribeTiersRequest, DescribeTiersResponse, LlmRouterService, NoulAnswer,
        SystemOneService,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_TIMEOUT_SECONDS: i32 = 30;
    // A fixed owner so validate_tool tests can share draft_tool without minting a Uuid each time.
    const OWNER: Uuid = Uuid::from_u128(1);
    const REFUSAL_SENTENCE: &str =
        "the description does not say what changes, and the timeout looks implausible";

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

    async fn draft_tool(service: &Service) -> i64 {
        service
            .create_tool(
                Some(OWNER),
                a_tool("echo", "Echo", "Echoes input", Risk::ReadOnly),
            )
            .await
            .expect("create")
    }

    struct FakeLlmRouter {
        completions: Arc<AtomicUsize>,
    }

    #[allow(refining_impl_trait)]
    impl LlmRouterService for FakeLlmRouter {
        async fn complete(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CompleteRequest>,
        ) -> ServiceResult<CompleteResponse> {
            self.completions.fetch_add(1, Ordering::SeqCst);
            Response::ok(CompleteResponse {
                content: REFUSAL_SENTENCE.to_owned(),
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

    struct FakeSystemOne {
        noul: f64,
    }

    #[allow(refining_impl_trait)]
    impl SystemOneService for FakeSystemOne {
        async fn decide(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DecideRequest>,
        ) -> ServiceResult<DecideResponse> {
            Response::ok(DecideResponse {
                answers: vec![Answer {
                    id: "meets_standard".to_owned(),
                    answer: NoulAnswer {
                        noul: self.noul,
                        ..Default::default()
                    }
                    .into(),
                    ..Default::default()
                }],
                ..Default::default()
            })
        }

        async fn describe_models(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, DescribeModelsRequest>,
        ) -> ServiceResult<DescribeModelsResponse> {
            Response::ok(DescribeModelsResponse::default())
        }
    }

    // The same URL now serves both LlmRouterService and SystemOneService, so the fake mounts both
    // on one router. `completions` counts Complete calls, so a test can prove a path made none.
    async fn serve_llm_counting_decisions(noul: f64) -> (String, Arc<AtomicUsize>) {
        let completions = Arc::new(AtomicUsize::new(0));
        let llm = Arc::new(FakeLlmRouter {
            completions: completions.clone(),
        });
        let decider = Arc::new(FakeSystemOne { noul });
        let connect = ConnectRouter::new().add_service(llm).add_service(decider);
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://{address}"), completions)
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
    async fn a_confident_yes_validates_without_asking_for_words() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(approved);
        assert!(
            feedback.is_empty(),
            "an approval needs no prose: {feedback:?}"
        );
        assert_eq!(
            completions.load(Ordering::SeqCst),
            0,
            "Complete was called on the happy path"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refusal_spends_one_completion_on_the_reason() {
        let (llm_url, completions) = serve_llm_counting_decisions(0.08).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved);
        assert!(!feedback.is_empty());
        assert_eq!(completions.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unreachable_decider_refuses_rather_than_approving() {
        let (_test, service) = service_with("http://127.0.0.1:1").await;
        let tool_id = draft_tool(&service).await;
        assert!(service.validate_tool(tool_id, OWNER).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_tool_rejects_a_user_owned_system_slug() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, feedback) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(approved);
        assert!(feedback.is_empty());
        let row = crate::entity::tool::Entity::find_by_id(tool_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(row.status, Status::Validated);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejects_a_tool_that_is_no_longer_draft() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;
        service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("first validate");

        let error = service.validate_tool(tool_id, OWNER).await.unwrap_err();

        assert!(matches!(error, ToolError::InvalidRequest(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn validate_tool_rejection_stays_draft() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.08).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        let (approved, _) = service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");

        assert!(!approved);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn activate_tool_requires_validated_status() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
        let (_test, service) = service_with(&llm_url).await;
        let tool_id = draft_tool(&service).await;

        assert!(
            service.activate_tool(tool_id, OWNER).await.is_err(),
            "still Draft, not Validated"
        );

        service
            .validate_tool(tool_id, OWNER)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, OWNER)
            .await
            .expect("activate");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_tools_hides_other_users_tools() {
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
        let (llm_url, _completions) = serve_llm_counting_decisions(0.93).await;
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
