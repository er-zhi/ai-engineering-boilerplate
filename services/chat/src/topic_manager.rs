// create_topic: the entry point for a new topic, root or child. Concurrency-limited per
// session (spec's default of 3 running at once) — beyond the limit, a topic is Queued instead
// of started; Task 8's completion handler promotes the oldest Queued topic when a slot frees.

use chrono::Utc;
use common::proto::chat::v1::ChatEvent;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait,
    PaginatorTrait, QueryFilter,
};
use uuid::Uuid;

use crate::engine_client::EngineClient;
use crate::entity::topic::{self, Status};
use crate::error::ChatError;
use crate::events::EventBus;
use crate::session_manager::SessionManager;

const AGENT_GRAPH_ID: &str = "agent";
pub const MAX_CONCURRENT_TOPICS: u64 = 3;

pub struct TopicManager {
    pub(crate) session: SessionManager,
    pub(crate) engine: EngineClient,
    pub(crate) events: EventBus,
}

impl TopicManager {
    pub fn new(db: DatabaseConnection, engine_url: &str) -> Result<Self, String> {
        Ok(Self {
            session: SessionManager::new(db),
            engine: EngineClient::new(engine_url)?,
            events: EventBus::default(),
        })
    }

    pub async fn create_topic(
        &self,
        user_id: Uuid,
        parent_id: Option<i64>,
        title: String,
        input_json: String,
    ) -> Result<(i64, Status), ChatError> {
        let session = self.session.get_or_create_session(user_id).await?;
        let running = topic::Entity::find()
            .filter(topic::Column::SessionId.eq(session.id))
            .filter(topic::Column::Status.eq(Status::Running))
            .count(&self.session.db)
            .await?;

        let now = Utc::now();
        let status = if running < MAX_CONCURRENT_TOPICS {
            Status::Running
        } else {
            Status::Queued
        };
        let execution_id = if status == Status::Running {
            Some(
                self.engine
                    .start_execution(AGENT_GRAPH_ID, &input_json)
                    .await
                    .map_err(ChatError::Engine)?,
            )
        } else {
            None
        };

        let row = topic::ActiveModel {
            session_id: Set(session.id),
            parent_id: Set(parent_id),
            title: Set(title),
            status: Set(status),
            execution_id: Set(execution_id),
            result_summary: Set(None),
            artifact_ids: Set(serde_json::json!([])),
            created_at: Set(now),
            updated_at: Set(now),
            ..Default::default()
        }
        .insert(&self.session.db)
        .await?;

        if session.focus_topic_id.is_none() {
            self.set_focus_silently(session.id, row.id).await?;
        }

        self.events.publish(ChatEvent {
            topic_id: row.id.to_string(),
            kind: if status == Status::Running {
                "topic_started".to_owned()
            } else {
                "topic_queued".to_owned()
            }
            .to_owned(),
            occurred_at: now.to_rfc3339(),
            ..Default::default()
        });

        Ok((row.id, status))
    }

    /// Used only by `create_topic` for the "first topic in a session becomes focus" rule (spec)
    /// — doesn't emit `focus_changed` itself (there was no prior focus to change from). Task 8's
    /// `set_focus` is the user-facing operation and does emit it.
    async fn set_focus_silently(&self, session_id: Uuid, topic_id: i64) -> Result<(), ChatError> {
        use crate::entity::session;
        let mut active: session::ActiveModel = session::Entity::find_by_id(session_id)
            .one(&self.session.db)
            .await?
            .ok_or(ChatError::InvalidRequest("session vanished".to_owned()))?
            .into();
        active.focus_topic_id = Set(Some(topic_id));
        active.update(&self.session.db).await?;
        Ok(())
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use common::proto::engine::v1::{
        CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse,
        EngineService, Execution, ExecutionEvent, GetExecutionRequest, InterruptRequest,
        InterruptResponse, ListSchedulesRequest, ListSchedulesResponse, RegisterGraphRequest,
        RegisterGraphResponse, ResumeRequest, ResumeResponse, StartExecutionRequest,
        StartExecutionResponse, StreamEventsRequest as EngineStreamEventsRequest,
    };
    use connectrpc::{
        RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult,
        ServiceStream,
    };
    use std::sync::Arc;

    struct FakeEngine;

    #[allow(refining_impl_trait)]
    impl EngineService for FakeEngine {
        async fn register_graph(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, RegisterGraphRequest>,
        ) -> ServiceResult<RegisterGraphResponse> {
            Response::ok(RegisterGraphResponse::default())
        }
        async fn start_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, StartExecutionRequest>,
        ) -> ServiceResult<StartExecutionResponse> {
            Response::ok(StartExecutionResponse {
                execution_id: uuid::Uuid::new_v4().to_string(),
                ..Default::default()
            })
        }
        async fn interrupt(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, InterruptRequest>,
        ) -> ServiceResult<InterruptResponse> {
            Response::ok(InterruptResponse::default())
        }
        async fn resume(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ResumeRequest>,
        ) -> ServiceResult<ResumeResponse> {
            Response::ok(ResumeResponse::default())
        }
        async fn cancel(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CancelRequest>,
        ) -> ServiceResult<CancelResponse> {
            Response::ok(CancelResponse::default())
        }
        async fn get_execution(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, GetExecutionRequest>,
        ) -> ServiceResult<Execution> {
            Response::ok(Execution::default())
        }
        async fn stream_events(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, EngineStreamEventsRequest>,
        ) -> ServiceResult<ServiceStream<ExecutionEvent>> {
            Response::stream_ok(futures::stream::empty())
        }
        async fn create_schedule(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, CreateScheduleRequest>,
        ) -> ServiceResult<CreateScheduleResponse> {
            Response::ok(CreateScheduleResponse::default())
        }
        async fn list_schedules(
            &self,
            _ctx: RequestContext,
            _request: ServiceRequest<'_, ListSchedulesRequest>,
        ) -> ServiceResult<ListSchedulesResponse> {
            Response::ok(ListSchedulesResponse::default())
        }
    }

    async fn serve_engine() -> String {
        let connect = ConnectRouter::new().add_service(Arc::new(FakeEngine));
        let app = axum::Router::new().fallback_service(connect.into_axum_service());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("addr");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        format!("http://{address}")
    }

    async fn manager_with() -> (crate::test_db::TestDb, TopicManager) {
        let test = crate::test_db::start().await;
        let engine_url = serve_engine().await;
        let manager = TopicManager::new(test.db.clone(), &engine_url).expect("manager");
        (test, manager)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_first_topic_in_a_session_starts_running_and_becomes_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();

        let (topic_id, status) = manager
            .create_topic(
                user_id,
                None,
                "First".into(),
                r#"{"question": "hi"}"#.into(),
            )
            .await
            .expect("create");

        assert_eq!(status, Status::Running);
        let (focus, _topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(topic_id));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fourth_concurrent_topic_is_queued_not_started() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();
        for i in 0..MAX_CONCURRENT_TOPICS {
            let (_id, status) = manager
                .create_topic(user_id, None, format!("T{i}"), "{}".into())
                .await
                .expect("create");
            assert_eq!(status, Status::Running);
        }

        let (_id, status) = manager
            .create_topic(user_id, None, "Overflow".into(), "{}".into())
            .await
            .expect("create");

        assert_eq!(status, Status::Queued);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_second_root_topic_does_not_steal_focus() {
        let (_test, manager) = manager_with().await;
        let user_id = Uuid::new_v4();
        let (first_id, _) = manager
            .create_topic(user_id, None, "First".into(), "{}".into())
            .await
            .expect("create");

        manager
            .create_topic(user_id, None, "Second".into(), "{}".into())
            .await
            .expect("create");

        let (focus, _topics) = manager
            .session
            .get_session_view(user_id)
            .await
            .expect("view");
        assert_eq!(focus, Some(first_id));
    }
}
