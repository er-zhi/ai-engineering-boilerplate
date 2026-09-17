// The in-process Engine and llm-router stand-ins every topic test builds a TopicManager on.

use chrono::Utc;
use common::principal::Principal;
use common::proto::engine::v1::{
    CancelRequest, CancelResponse, CreateScheduleRequest, CreateScheduleResponse, EngineService,
    Execution, ExecutionEvent, ExecutionEventKind, GetExecutionRequest, InterruptRequest,
    InterruptResponse, ListSchedulesRequest, ListSchedulesResponse, RegisterGraphRequest,
    RegisterGraphResponse, ResumeRequest, ResumeResponse, StartExecutionRequest,
    StartExecutionResponse, StreamEventsRequest as EngineStreamEventsRequest,
};
use common::proto::llm_router::v1::{
    CompleteRequest, CompleteResponse, DescribeTiersRequest, DescribeTiersResponse,
    LlmRouterService,
};
use connectrpc::{
    RequestContext, Response, Router as ConnectRouter, ServiceRequest, ServiceResult, ServiceStream,
};
use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use uuid::Uuid;

use crate::engine_client::EngineEvent;
use crate::entity::topic::{self, Status};
use crate::topic_manager::TopicManager;

#[derive(Default)]
pub struct FakeEngine {
    pub interrupts: Mutex<Vec<InterruptRequest>>,
    pub interrupt_failures_remaining: AtomicUsize,
    pub start_executions: Mutex<Vec<StartExecutionRequest>>,
    pub principals: Mutex<Vec<Option<Principal>>>,
    pub completions: Mutex<HashMap<String, ExecutionEvent>>,
    pub scripted_streams: Mutex<HashMap<String, Vec<Vec<ExecutionEvent>>>>,
    pub stream_calls: Mutex<HashMap<String, u32>>,
}

impl FakeEngine {
    pub fn arm_scripted_stream(&self, execution_id: Uuid, calls: Vec<Vec<ExecutionEvent>>) {
        self.scripted_streams
            .lock()
            .expect("lock")
            .insert(execution_id.to_string(), calls);
    }

    pub fn stream_call_count(&self, execution_id: Uuid) -> u32 {
        self.stream_calls
            .lock()
            .expect("lock")
            .get(&execution_id.to_string())
            .copied()
            .unwrap_or(0)
    }

    pub fn start_execution_count(&self) -> usize {
        self.start_executions.lock().expect("lock").len()
    }

    pub fn interrupt_count(&self) -> usize {
        self.interrupts.lock().expect("lock").len()
    }

    fn record(&self, ctx: &RequestContext) {
        self.principals
            .lock()
            .expect("lock")
            .push(common::principal::from_metadata(ctx.headers()));
    }
}

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
        ctx: RequestContext,
        request: ServiceRequest<'_, StartExecutionRequest>,
    ) -> ServiceResult<StartExecutionResponse> {
        self.record(&ctx);
        self.start_executions
            .lock()
            .expect("lock")
            .push(request.to_owned_message());
        Response::ok(StartExecutionResponse {
            execution_id: Uuid::new_v4().to_string(),
            ..Default::default()
        })
    }
    async fn interrupt(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, InterruptRequest>,
    ) -> ServiceResult<InterruptResponse> {
        self.record(&ctx);
        self.interrupts
            .lock()
            .expect("lock")
            .push(request.to_owned_message());
        if self
            .interrupt_failures_remaining
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(connectrpc::ConnectError::unavailable(
                "configured fake engine interrupt failure",
            ));
        }
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
        ctx: RequestContext,
        request: ServiceRequest<'_, EngineStreamEventsRequest>,
    ) -> ServiceResult<ServiceStream<ExecutionEvent>> {
        self.record(&ctx);
        let owned = request.to_owned_message();
        let Some(execution_id) = owned.execution_id else {
            return Response::stream_ok(futures::stream::empty());
        };

        let call_index = {
            let mut calls = self.stream_calls.lock().expect("lock");
            let entry = calls.entry(execution_id.clone()).or_insert(0);
            let index = *entry;
            *entry += 1;
            index
        };

        if let Some(scripts) = self
            .scripted_streams
            .lock()
            .expect("lock")
            .get(&execution_id)
        {
            let index = (call_index as usize).min(scripts.len().saturating_sub(1));
            let events = scripts.get(index).cloned().unwrap_or_default();
            return Response::stream_ok(futures::stream::iter(
                events.into_iter().map(Ok).collect::<Vec<_>>(),
            ));
        }

        let event = self
            .completions
            .lock()
            .expect("lock")
            .get(&execution_id)
            .cloned();
        match event {
            Some(event) => Response::stream_ok(futures::stream::iter([Ok(event)])),
            None => Response::stream_ok(futures::stream::empty()),
        }
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

#[derive(Default)]
pub struct FakeLlmRouter {
    pub answer: Mutex<Option<String>>,
    pub prompts: Mutex<Vec<String>>,
    pub meeting: Mutex<Option<Arc<tokio::sync::Barrier>>>,
}

impl FakeLlmRouter {
    pub fn answer_with(&self, content: &str) {
        *self.answer.lock().expect("lock") = Some(content.to_owned());
    }

    pub fn hold_every_call_until(&self, meeting: Arc<tokio::sync::Barrier>) {
        *self.meeting.lock().expect("lock") = Some(meeting);
    }
}

#[allow(refining_impl_trait)]
impl LlmRouterService for FakeLlmRouter {
    async fn complete(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CompleteRequest>,
    ) -> ServiceResult<CompleteResponse> {
        let owned = request.to_owned_message();
        self.prompts.lock().expect("lock").push(owned.user_prompt);
        let meeting = self.meeting.lock().expect("lock").clone();
        if let Some(meeting) = meeting {
            meeting.wait().await;
        }
        match self.answer.lock().expect("lock").clone() {
            Some(content) => Response::ok(CompleteResponse {
                content,
                ..Default::default()
            }),
            None => Err(connectrpc::ConnectError::unavailable(
                "configured fake llm-router failure",
            )),
        }
    }
    async fn describe_tiers(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, DescribeTiersRequest>,
    ) -> ServiceResult<DescribeTiersResponse> {
        Response::ok(DescribeTiersResponse::default())
    }
}

async fn serve(connect: ConnectRouter) -> String {
    let app = axum::Router::new().fallback_service(connect.into_axum_service());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    format!("http://{address}")
}

pub async fn manager_with() -> (crate::test_db::TestDb, Arc<TopicManager>, Arc<FakeEngine>) {
    let (test, manager, engine, _router) = manager_with_router().await;
    (test, manager, engine)
}

pub async fn manager_with_router() -> (
    crate::test_db::TestDb,
    Arc<TopicManager>,
    Arc<FakeEngine>,
    Arc<FakeLlmRouter>,
) {
    let test = crate::test_db::start().await;
    let fake = Arc::new(FakeEngine::default());
    let router = Arc::new(FakeLlmRouter::default());
    let engine_url = serve(ConnectRouter::new().add_service(Arc::clone(&fake))).await;
    let router_url = serve(ConnectRouter::new().add_service(Arc::clone(&router))).await;
    let manager = Arc::new(
        TopicManager::new(test.db.clone(), &engine_url, &router_url)
            .expect("manager")
            .with_reconnect_delays(Duration::from_millis(1), Duration::from_millis(20)),
    );
    (test, manager, fake, router)
}

pub async fn arm_completed_event(
    manager: &Arc<TopicManager>,
    fake: &FakeEngine,
    topic_id: i64,
    answer: &str,
) -> Uuid {
    let execution_id = execution_id_of(manager, topic_id).await;
    arm(
        fake,
        execution_id,
        completed_event_for(execution_id, 1, answer),
    );
    execution_id
}

pub async fn arm_failed_event(
    manager: &Arc<TopicManager>,
    fake: &FakeEngine,
    topic_id: i64,
    error: &str,
) -> Uuid {
    let execution_id = execution_id_of(manager, topic_id).await;
    arm(
        fake,
        execution_id,
        ExecutionEvent {
            id: "e1".to_owned(),
            execution_id: execution_id.to_string(),
            payload_kind: ExecutionEventKind::ExecutionFailed.into(),
            payload_json: serde_json::json!({ "error": error }).to_string(),
            error: Some(error.to_owned()),
            ..Default::default()
        },
    );
    execution_id
}

pub async fn arm_cancelled_event(
    manager: &Arc<TopicManager>,
    fake: &FakeEngine,
    topic_id: i64,
) -> Uuid {
    let execution_id = execution_id_of(manager, topic_id).await;
    arm(
        fake,
        execution_id,
        ExecutionEvent {
            id: "e1".to_owned(),
            execution_id: execution_id.to_string(),
            payload_kind: ExecutionEventKind::ExecutionCancelled.into(),
            payload_json: "{}".to_owned(),
            ..Default::default()
        },
    );
    execution_id
}

fn arm(fake: &FakeEngine, execution_id: Uuid, event: ExecutionEvent) {
    fake.completions
        .lock()
        .expect("lock")
        .insert(execution_id.to_string(), event);
}

pub async fn execution_id_of(manager: &Arc<TopicManager>, topic_id: i64) -> Uuid {
    topic_row(manager, topic_id)
        .await
        .execution_id
        .expect("running topic has an execution_id")
}

pub async fn topic_row(manager: &Arc<TopicManager>, topic_id: i64) -> topic::Model {
    topic::Entity::find_by_id(topic_id)
        .one(manager.db())
        .await
        .expect("query")
        .expect("topic")
}

pub fn one_topic(topic_ids: Vec<i64>) -> i64 {
    assert_eq!(topic_ids.len(), 1, "expected one topic: {topic_ids:?}");
    topic_ids[0]
}

pub async fn focus_of(manager: &Arc<TopicManager>, user_id: Uuid) -> Option<i64> {
    manager
        .session
        .get_session_view(user_id)
        .await
        .expect("view")
        .0
}

pub async fn set_status(manager: &Arc<TopicManager>, topic_id: i64, status: Status) {
    let mut active: topic::ActiveModel = topic_row(manager, topic_id).await.into();
    active.status = Set(status);
    active.update(manager.db()).await.expect("update");
}

pub fn completed_event() -> EngineEvent {
    EngineEvent {
        id: Uuid::new_v4().to_string(),
        ..EngineEvent::from(completed_event_for(Uuid::new_v4(), 1, "done"))
    }
}

pub fn node_completed_event(execution_id: Uuid, version: i32) -> ExecutionEvent {
    ExecutionEvent {
        id: format!("e{version}"),
        execution_id: execution_id.to_string(),
        version,
        payload_kind: ExecutionEventKind::NodeCompleted.into(),
        payload_json: r#"{"node":"search"}"#.to_owned(),
        occurred_at: Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

pub fn completed_event_for(execution_id: Uuid, version: i32, answer: &str) -> ExecutionEvent {
    ExecutionEvent {
        id: format!("e{version}"),
        execution_id: execution_id.to_string(),
        version,
        payload_kind: ExecutionEventKind::ExecutionCompleted.into(),
        payload_json: serde_json::json!({"final_state": {}, "result": answer}).to_string(),
        result: Some(answer.to_owned()),
        occurred_at: Utc::now().to_rfc3339(),
        ..Default::default()
    }
}

pub async fn insert_unwatched_running_topic(
    manager: &Arc<TopicManager>,
    user_id: Uuid,
) -> (i64, Uuid) {
    let session = manager.session_of_user(user_id).await.expect("session");
    let execution_id = Uuid::new_v4();
    let now = Utc::now();
    let row = topic::ActiveModel {
        session_id: Set(session.id),
        parent_id: Set(None),
        title: Set("First".to_owned()),
        status: Set(Status::Running),
        execution_id: Set(Some(execution_id)),
        input_json: Set(serde_json::json!({})),
        result_summary: Set(None),
        artifact_ids: Set(serde_json::json!([])),
        created_at: Set(now),
        updated_at: Set(now),
        ..Default::default()
    }
    .insert(manager.db())
    .await
    .expect("insert topic");
    (row.id, execution_id)
}
