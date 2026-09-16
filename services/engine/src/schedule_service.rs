// The CreateSchedule/ListSchedules half of `Service` — the same type as service.rs's, split into
// its own file so one file doesn't carry two subsystems. Everything here is request handling:
// validate, insert, read back. Firing a schedule is scheduler.rs's job, and what it fires is the
// ordinary `start_execution` below it, not a private path.

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ActiveValue::Set, ColumnTrait, EntityTrait, QueryFilter, QueryOrder,
};
use serde_json::Value;
use uuid::Uuid;

use crate::cron_next::next_fire_after;
use crate::entity::schedule;
use crate::error::EngineError;
use crate::service::Service;

impl Service {
    /// Registers a cron schedule. `cron_expr` is validated by computing its first fire time,
    /// which doubles as the row's initial `next_run_at` — an expression that can't produce one is
    /// rejected here rather than becoming a row the scheduler can only log about.
    pub async fn create_schedule(
        &self,
        graph_id: String,
        version: Option<i32>,
        cron_expr: &str,
        input_json: &str,
        user_id: Option<Uuid>,
    ) -> Result<i64, EngineError> {
        let next_run_at =
            next_fire_after(cron_expr, Utc::now()).map_err(EngineError::InvalidRequest)?;
        // Same rule as start_execution's: execution state is an object everywhere downstream, so
        // a scalar or array input is rejected now instead of failing on every single fire.
        let input: Value = serde_json::from_str(input_json)
            .map_err(|e| EngineError::InvalidRequest(e.to_string()))?;
        if !input.is_object() {
            return Err(EngineError::InvalidRequest(format!(
                "input_json must decode to a JSON object, got: {input_json}"
            )));
        }

        let row = schedule::ActiveModel {
            graph_id: Set(graph_id),
            graph_version: Set(version),
            cron_expr: Set(cron_expr.to_owned()),
            input_json: Set(input),
            user_id: Set(user_id),
            enabled: Set(true),
            next_run_at: Set(next_run_at),
            last_execution_id: Set(None),
            created_at: Set(Utc::now()),
            updated_at: Set(Utc::now()),
            ..Default::default()
        }
        .insert(&self.db)
        .await?;
        Ok(row.id)
    }

    /// Every schedule belonging to `user_id` — and, for a caller with no Principal, the system
    /// schedules (`user_id IS NULL`) rather than everybody's. The caller's identity comes from
    /// request metadata in main.rs, never from a body field.
    pub async fn list_schedules(
        &self,
        user_id: Option<Uuid>,
    ) -> Result<Vec<schedule::Model>, EngineError> {
        let query = schedule::Entity::find();
        let query = match user_id {
            Some(id) => query.filter(schedule::Column::UserId.eq(id)),
            None => query.filter(schedule::Column::UserId.is_null()),
        };
        Ok(query
            .order_by_asc(schedule::Column::NextRunAt)
            .all(&self.db)
            .await?)
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;

    const HOURLY: &str = "0 0 * * * *";

    #[tokio::test(flavor = "multi_thread")]
    async fn create_schedule_stores_the_first_fire_time_as_next_run_at() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());

        let id = service
            .create_schedule(
                "simple".to_owned(),
                None,
                HOURLY,
                r#"{"question": "hi"}"#,
                None,
            )
            .await
            .expect("create");

        let row = schedule::Entity::find_by_id(id)
            .one(&test.db)
            .await
            .expect("query")
            .expect("row");
        assert!(row.enabled);
        assert!(row.next_run_at > Utc::now(), "the first fire time is ahead");
        assert_eq!(row.input_json, serde_json::json!({"question": "hi"}));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn create_schedule_rejects_a_bad_expression_and_a_non_object_input() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());

        assert!(matches!(
            service
                .create_schedule("simple".to_owned(), None, "every hour please", "{}", None)
                .await,
            Err(EngineError::InvalidRequest(_))
        ));
        assert!(matches!(
            service
                .create_schedule("simple".to_owned(), None, HOURLY, "5", None)
                .await,
            Err(EngineError::InvalidRequest(_))
        ));
        assert!(
            schedule::Entity::find()
                .all(&test.db)
                .await
                .expect("query")
                .is_empty(),
            "neither rejection left a row behind"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn list_schedules_only_returns_the_callers_own() {
        let test = crate::test_db::start().await;
        let service = Service::new(test.db.clone());
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        service
            .create_schedule("simple".to_owned(), None, HOURLY, "{}", Some(mine))
            .await
            .expect("create mine");
        service
            .create_schedule("simple".to_owned(), None, HOURLY, "{}", Some(theirs))
            .await
            .expect("create theirs");
        service
            .create_schedule("simple".to_owned(), None, HOURLY, "{}", None)
            .await
            .expect("create a system one");

        let listed = service.list_schedules(Some(mine)).await.expect("list");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].user_id, Some(mine));
    }
}
