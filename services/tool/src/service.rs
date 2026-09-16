// CreateTool/ValidateTool/ActivateTool/ListTools: the lifecycle a user-created tool goes
// through before it's callable — Execute (Task 10) is the only method that actually runs one.

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
use std::time::Duration;
use uuid::Uuid;

use crate::entity::tool::{ActiveModel, Entity, Risk, Status};
use crate::error::ToolError;
use crate::providers::brave::{BraveSearchProvider, SearchProvider};
use crate::tools::kb_client::KnowledgeBaseClient;

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);
const VALIDATE_SYSTEM_PROMPT: &str = r#"You validate a tool definition against 2026 API design standards. You will be given a tool's name, description, JSON Schema input_schema, JSON Schema output_schema, risk level, and timeout_seconds. Respond with exactly this JSON and nothing else: {"approved": true or false, "feedback": "one or two sentences"}. Approve only if input_schema and output_schema are each a plausible JSON Schema object, description clearly states what the tool does (and, for risk "write" or "destructive", what it changes), and timeout_seconds is between 1 and 300."#;

pub struct Service {
    db: DatabaseConnection,
    llm: LlmRouterServiceClient<HttpClient>,
    search: BraveSearchProvider,
    http: reqwest::Client,
    kb: KnowledgeBaseClient,
}

impl Service {
    pub fn new(
        db: DatabaseConnection,
        llm_router_url: &str,
        brave_api_key: String,
        knowledge_base_url: &str,
    ) -> Result<Self, String> {
        let target = llm_router_url
            .parse()
            .map_err(|e| format!("could not parse LLM_ROUTER_URL {llm_router_url:?}: {e}"))?;
        Ok(Self {
            db,
            llm: LlmRouterServiceClient::new(
                HttpClient::plaintext_http2_only(),
                ClientConfig::new(target)
                    .with_protocol(Protocol::Grpc)
                    .with_default_timeout(VALIDATE_TIMEOUT)
                    .proto(),
            ),
            search: BraveSearchProvider::new(brave_api_key),
            http: reqwest::Client::new(),
            kb: KnowledgeBaseClient::new(knowledge_base_url)?,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn create_tool(
        &self,
        user_id: Option<Uuid>,
        slug: String,
        name: String,
        description: String,
        input_schema: Value,
        output_schema: Value,
        risk: Risk,
        timeout_seconds: i32,
    ) -> Result<i64, ToolError> {
        if !input_schema.is_object() || !output_schema.is_object() {
            return Err(ToolError::InvalidRequest(
                "input_schema and output_schema must be JSON objects".to_owned(),
            ));
        }
        // A user-created tool cannot claim a system tool's slug: `execute`/`run_system_tool`
        // dispatch the real system implementation by slug string alone, so a user row sharing
        // one of these names would run as the real web_search/web_fetch/kb_search/
        // kb_read_document — silently bypassing that row's own Draft/Validated/Active lifecycle
        // (seeding, which legitimately owns these slugs, passes `user_id: None` and skips this
        // check).
        if user_id.is_some() && crate::slugs::ALL.contains(&slug.as_str()) {
            return Err(ToolError::InvalidRequest(format!(
                "slug {slug:?} is reserved for a system tool"
            )));
        }
        let now = Utc::now();
        let row = ActiveModel {
            user_id: Set(user_id),
            slug: Set(slug),
            name: Set(name),
            description: Set(description),
            input_schema: Set(input_schema),
            output_schema: Set(output_schema),
            connection_id: Set(None),
            risk: Set(risk),
            timeout_seconds: Set(timeout_seconds),
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
        let tool = self.owned_tool(tool_id, user_id).await?;
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
                system_prompt: VALIDATE_SYSTEM_PROMPT.to_owned(),
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
        let parsed: Value = serde_json::from_str(&response.content).unwrap_or(Value::Null);
        let approved = parsed
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let feedback = parsed
            .get("feedback")
            .and_then(Value::as_str)
            .unwrap_or("the model did not return the expected JSON shape")
            .to_owned();

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
        let tool = self.owned_tool(tool_id, user_id).await?;
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

    /// The catalog `Dispatcher` (Task 14) injects into an LLM's tool-calling prompt — only
    /// `Active` tools are eligible, since `Draft`/`Validated` rows haven't cleared review yet.
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

    /// A tool the caller owns — used by `validate_tool`/`activate_tool`, which only ever act on
    /// the caller's own `Draft`/`Validated` tools (a system tool's `user_id` never matches any
    /// `Principal`, so this naturally rejects attempts to validate/activate one).
    async fn owned_tool(
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
        self.run_system_tool(slug, &input).await
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

    // Split by slug into one small helper per system tool (rather than one long match arm body)
    // purely to stay under this workspace's clippy::too_many_lines — semantically this is still
    // exactly the four-way dispatch the brief lays out.
    async fn run_system_tool(
        &self,
        slug: &str,
        input: &Value,
    ) -> Result<ExecuteOutcome, ToolError> {
        match slug {
            crate::slugs::WEB_SEARCH => self.run_web_search(input).await,
            crate::slugs::WEB_FETCH => self.run_web_fetch(input).await,
            crate::slugs::KB_SEARCH => self.run_kb_search(input).await,
            crate::slugs::KB_READ_DOCUMENT => self.run_kb_read_document(input).await,
            other => Ok(ExecuteOutcome::NotExecutable(format!(
                "tool {other:?} has no runnable implementation yet — user-created tools need Integrations Service"
            ))),
        }
    }

    async fn run_web_search(&self, input: &Value) -> Result<ExecuteOutcome, ToolError> {
        let (query, limit) = query_and_limit(input);
        Ok(match self.search.search(query, limit).await {
            Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
            Err(error) => ExecuteOutcome::Error(error),
        })
    }

    async fn run_web_fetch(&self, input: &Value) -> Result<ExecuteOutcome, ToolError> {
        let url = input.get("url").and_then(Value::as_str).unwrap_or_default();
        if let Err(error) = crate::tools::web_fetch::ensure_public_url(url).await {
            return Ok(ExecuteOutcome::Error(error));
        }
        Ok(
            match crate::tools::web_fetch::fetch(&self.http, url).await {
                Ok(page) => ExecuteOutcome::Ok(serde_json::to_value(page)?),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }

    async fn run_kb_search(&self, input: &Value) -> Result<ExecuteOutcome, ToolError> {
        let (query, limit) = query_and_limit(input);
        Ok(match self.kb.search(query, limit).await {
            Ok(results) => ExecuteOutcome::Ok(serde_json::to_value(results)?),
            Err(error) => ExecuteOutcome::Error(error),
        })
    }

    async fn run_kb_read_document(&self, input: &Value) -> Result<ExecuteOutcome, ToolError> {
        let source = input
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source_id = input
            .get("source_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let version = input
            .get("version")
            .and_then(Value::as_str)
            .unwrap_or_default();
        Ok(
            match self.kb.read_document(source, source_id, version).await {
                Ok(content) => ExecuteOutcome::Ok(serde_json::json!({"content": content})),
                Err(error) => ExecuteOutcome::Error(error),
            },
        )
    }
}

/// Shared by `run_web_search` and `run_kb_search`: both slugs take the same `{query, limit}`
/// shape, `limit` defaulting to 5 and capped at 20.
fn query_and_limit(input: &Value) -> (&str, u8) {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .filter(|&v| v > 0)
        .map_or(5, |v| u8::try_from(Ord::min(v, 20)).unwrap_or(20));
    (query, limit)
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
                crate::slugs::WEB_SEARCH.into(),
                "Shadow web search".into(),
                "d".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
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
                "echo".into(),
                "Echo".into(),
                "Echoes input".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
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
                "echo".into(),
                "Echo".into(),
                "Echoes input".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
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
    async fn validate_tool_rejection_stays_draft() {
        let llm_url =
            serve_llm(r#"{"approved": false, "feedback": "description is unclear"}"#).await;
        let (_test, service) = service_with(&llm_url).await;
        let user_id = Uuid::new_v4();
        let tool_id = service
            .create_tool(
                Some(user_id),
                "echo".into(),
                "Echo".into(),
                "".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
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
                "echo".into(),
                "Echo".into(),
                "Echoes input".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
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
        let tool_id = service
            .create_tool(
                Some(owner),
                "mine".into(),
                "Mine".into(),
                "d".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
            )
            .await
            .expect("create");
        // list_tools only ever returns Active rows (see its doc comment) — activate first so
        // this genuinely exercises the ownership filter rather than passing vacuously because
        // a Draft row is excluded from every caller's view regardless of who owns it.
        service
            .validate_tool(tool_id, owner)
            .await
            .expect("validate");
        service
            .activate_tool(tool_id, owner)
            .await
            .expect("activate");

        let seen_by_stranger = service.list_tools(Some(stranger)).await.expect("list");
        let seen_by_owner = service.list_tools(Some(owner)).await.expect("list");

        assert!(seen_by_stranger.iter().all(|t| t.slug != "mine"));
        assert!(seen_by_owner.iter().any(|t| t.slug == "mine"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn execute_on_a_read_only_system_tool_runs_it() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
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
    async fn execute_on_an_unregistered_slug_is_an_error() {
        let llm_url = serve_llm(r#"{"approved": true, "feedback": "ok"}"#).await;
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
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
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                "send_email".into(),
                "Send Email".into(),
                "Sends an email".into(),
                json!({}),
                json!({}),
                Risk::Write,
                30,
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
        let kb_url = crate::tools::kb_client::tests_support::serve_kb().await;
        let (_test, service) = full_service_with(&llm_url, &kb_url).await;
        let user_id = Uuid::new_v4();
        service
            .create_tool(
                Some(user_id),
                "custom_thing".into(),
                "Custom".into(),
                "d".into(),
                json!({}),
                json!({}),
                Risk::ReadOnly,
                30,
            )
            .await
            .expect("create");

        let outcome = service
            .execute(Some(user_id), "custom_thing", "{}", "e:c:0")
            .await
            .expect("execute");

        assert!(matches!(outcome, ExecuteOutcome::NotExecutable(_)));
    }

    async fn seed_system_tools(service: &Service) {
        // Mirrors Task 11's real seeding, scoped to just what Task 10's tests need.
        service
            .create_tool(
                None, // system tool — create_tool takes Option<Uuid> exactly so this is expressible
                crate::slugs::KB_SEARCH.into(),
                "Knowledge base search".into(),
                "Searches the knowledge base".into(),
                json!({"type": "object"}),
                json!({"type": "object"}),
                Risk::ReadOnly,
                30,
            )
            .await
            .expect("seed kb_search");
    }
}
