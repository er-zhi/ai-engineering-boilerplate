// Interrupts, resumes and cancels an execution that already exists.

use serde_json::Value;
use uuid::Uuid;

use crate::entity;
use crate::error::EngineError;
use crate::service::{Service, to_execution};

const RESUME_EVENT_STATE_KEY: &str = "resume_event";

impl Service {
    async fn load_idle(&self, execution_id: Uuid) -> Result<engine_core::Execution, EngineError> {
        let row = self.execution_row(execution_id).await?;
        if row.status == entity::execution::Status::Running {
            return Err(EngineError::Busy(format!(
                "execution {execution_id} is mid-tick, retry shortly"
            )));
        }
        let checkpoint = self.latest_checkpoint(execution_id).await?;
        to_execution(&row, checkpoint)
    }

    async fn persist(
        &self,
        execution_id: Uuid,
        next_step: u32,
        execution: &engine_core::Execution,
        events: &[engine_core::ExecutionEvent],
    ) -> Result<(), EngineError> {
        crate::store::commit_step(&self.db, execution_id, None, next_step, execution, events)
            .await?;
        Ok(())
    }

    pub async fn interrupt(&self, execution_id: Uuid, input_json: &str) -> Result<(), EngineError> {
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        let execution = self.load_idle(execution_id).await?;
        let next_step = self.next_checkpoint_step(execution_id).await?;
        let (execution, event) = engine_core::interrupt(execution, input);
        self.persist(
            execution_id,
            next_step,
            &execution,
            std::slice::from_ref(&event),
        )
        .await
    }

    pub async fn resume(&self, execution_id: Uuid, event_json: &str) -> Result<(), EngineError> {
        let event: Value = serde_json::from_str(event_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        let mut execution = self.load_idle(execution_id).await?;
        if !matches!(execution.status, engine_core::Status::Waiting(_)) {
            return Err(EngineError::InvalidRequest(format!(
                "execution {execution_id} is not waiting (status: {:?})",
                execution.status
            )));
        }
        let next_step = self.next_checkpoint_step(execution_id).await?;
        if let Some(object) = execution.state.as_object_mut() {
            object.insert(RESUME_EVENT_STATE_KEY.to_owned(), event);
        }
        execution.status = engine_core::Status::Ready;
        self.persist(execution_id, next_step, &execution, &[]).await
    }

    pub async fn cancel(&self, execution_id: Uuid) -> Result<(), EngineError> {
        let mut execution = self.load_idle(execution_id).await?;
        let next_step = self.next_checkpoint_step(execution_id).await?;
        execution.status = engine_core::Status::Cancelled;
        let event = execution.event(engine_core::ExecutionPayload::ExecutionCancelled);
        self.persist(
            execution_id,
            next_step,
            &execution,
            std::slice::from_ref(&event),
        )
        .await
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use chrono::Utc;
    use engine_core::{ActiveNode, Budget};
    use sea_orm::{ActiveModelTrait, ActiveValue::Set, EntityTrait};

    fn a_graph() -> String {
        serde_json::to_string(
            &engine_core::GraphBuilder::new()
                .entry("a")
                .task(
                    "a",
                    "noop",
                    serde_json::json!({"state_key": "reply", "reducer": "Replace"}),
                )
                .end("end")
                .edge("a", "end", engine_core::Condition::Always)
                .build("t", 0),
        )
        .expect("serialize")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interrupt_wakes_a_waiting_execution() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        let waiting_graph = serde_json::to_string(
            &engine_core::GraphBuilder::new()
                .entry("ask")
                .task("ask", "noop", serde_json::json!({}))
                .wait("pause", engine_core::WaitKind::UserInput)
                .build("waiter", 0),
        )
        .expect("serialize");
        service
            .register_graph("waiter".to_owned(), &waiting_graph)
            .await
            .expect("register");
        let execution_id = Uuid::new_v4();
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set("waiter".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(entity::execution::Status::Waiting),
            wait_kind: Set(Some(
                serde_json::to_value(engine_core::WaitKind::UserInput).unwrap(),
            )),
            current_nodes: Set(
                serde_json::to_value(vec![ActiveNode::plain(engine_core::NodeId("pause".into()))])
                    .unwrap(),
            ),
            iteration: Set(1),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .unwrap()),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&test.db)
        .await
        .expect("seed");

        service
            .interrupt(execution_id, r#"{"role": "user", "text": "hi"}"#)
            .await
            .expect("interrupt");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(execution.status, engine_core::Status::Ready);
        assert_eq!(
            execution.state.get("interrupt_input"),
            Some(&serde_json::json!({"role": "user", "text": "hi"}))
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn resume_refuses_an_execution_that_is_not_waiting() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        let execution_id = Uuid::new_v4();
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set("t".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(entity::execution::Status::Completed),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::json!([])),
            iteration: Set(3),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .unwrap()),
            lease_owner: Set(None),
            lease_until: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&test.db)
        .await
        .expect("seed");

        let result = service.resume(execution_id, "{}").await;

        assert!(matches!(result, Err(EngineError::InvalidRequest(_))));
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            row.status,
            entity::execution::Status::Completed,
            "a finished execution is not revived"
        );
    }

    // The caller's request was fine here — the execution is just mid-tick. This must answer
    // `Busy`, not `InvalidRequest`: the status a connect error carries is what tells Chat whether
    // this is worth telling the user to retry, or a request it should never resend unchanged.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_mid_tick_execution_is_refused_as_busy_not_invalid() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        let execution_id = Uuid::new_v4();
        entity::execution::ActiveModel {
            id: Set(execution_id),
            graph_id: Set("t".to_owned()),
            graph_version: Set(1),
            user_id: Set(None),
            status: Set(entity::execution::Status::Running),
            wait_kind: Set(None),
            current_nodes: Set(serde_json::json!([])),
            iteration: Set(1),
            max_iterations: Set(10),
            deadline: Set(None),
            budget: Set(serde_json::to_value(Budget::new(
                1000,
                10,
                std::time::Duration::from_secs(60),
            ))
            .unwrap()),
            lease_owner: Set(Some("worker-1".to_owned())),
            lease_until: Set(Some(Utc::now() + chrono::Duration::seconds(30))),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
        }
        .insert(&test.db)
        .await
        .expect("seed");

        let result = service.cancel(execution_id).await;

        assert!(matches!(result, Err(EngineError::Busy(_))), "{result:?}");
        let connect: connectrpc::ConnectError = result.unwrap_err().into();
        assert_eq!(connect.code, connectrpc::ConnectError::unavailable("").code);
        let row = entity::execution::Entity::find_by_id(execution_id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert_eq!(
            row.status,
            entity::execution::Status::Running,
            "a mid-tick execution is left alone, not cancelled out from under the worker running it"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_marks_the_execution_cancelled() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        service
            .register_graph("t".to_owned(), &a_graph())
            .await
            .expect("register");
        let execution_id = service
            .start_execution("t".to_owned(), None, "{}", None)
            .await
            .expect("start");

        service.cancel(execution_id).await.expect("cancel");

        let execution = service.get_execution(execution_id).await.expect("get");
        assert_eq!(execution.status, engine_core::Status::Cancelled);
    }
}
