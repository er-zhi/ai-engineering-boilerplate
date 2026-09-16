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

const VALIDATE_TIMEOUT: Duration = Duration::from_secs(60);
const VALIDATE_SYSTEM_PROMPT: &str = r#"You validate a tool definition against 2026 API design standards. You will be given a tool's name, description, JSON Schema input_schema, JSON Schema output_schema, risk level, and timeout_seconds. Respond with exactly this JSON and nothing else: {"approved": true or false, "feedback": "one or two sentences"}. Approve only if input_schema and output_schema are each a plausible JSON Schema object, description clearly states what the tool does (and, for risk "write" or "destructive", what it changes), and timeout_seconds is between 1 and 300."#;

pub struct Service {
    db: DatabaseConnection,
    llm: LlmRouterServiceClient<HttpClient>,
}

impl Service {
    pub fn new(db: DatabaseConnection, llm_router_url: &str) -> Result<Self, String> {
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

    pub async fn list_tools(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<Vec<crate::entity::tool::Model>, ToolError> {
        let mut query = Entity::find().filter(crate::entity::tool::Column::UserId.is_null());
        if let Some(id) = user_id {
            query = Entity::find().filter(
                crate::entity::tool::Column::UserId
                    .is_null()
                    .or(crate::entity::tool::Column::UserId.eq(id)),
            );
        }
        Ok(query.all(&self.db).await?)
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
        let service = Service::new(test.db.clone(), llm_url).expect("service");
        (test, service)
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
        service
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

        let seen_by_stranger = service.list_tools(Some(stranger)).await.expect("list");

        assert!(seen_by_stranger.iter().all(|t| t.slug != "mine"));
    }
}
