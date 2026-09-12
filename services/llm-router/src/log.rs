// Records what every call cost and how it went, keeping the statistics long and the payloads only as long as a post-mortem needs them.

use chrono::{DateTime, Duration, Utc};
use common::proto::llm_router::v1::{FinishReason, QualityTier};
use sea_orm::ActiveValue::Set;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, DbErr, EntityTrait, QueryFilter,
    TransactionTrait,
};
use serde_json::Value;

use crate::entity::request::Outcome;
use crate::entity::{payload, request};

const PAYLOAD_RETENTION_DAYS: i64 = 30;

pub struct Attempt {
    pub tier: QualityTier,
    pub model_used: String,
    pub used_backup: bool,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub latency_ms: i32,
    pub outcome: Outcome,
    pub finish_reason: FinishReason,
    pub sent: Value,
    pub received: Value,
}

pub trait RequestLog: Clone + Send + Sync + 'static {
    fn record(&self, attempt: Attempt) -> impl Future<Output = Result<(), DbErr>> + Send;
    fn drop_expired_payloads(
        &self,
        now: DateTime<Utc>,
    ) -> impl Future<Output = Result<u64, DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgRequestLog {
    db: DatabaseConnection,
}

impl PgRequestLog {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }
}

impl RequestLog for PgRequestLog {
    async fn record(&self, attempt: Attempt) -> Result<(), DbErr> {
        let now = Utc::now();
        let both_rows_or_neither = self.db.begin().await?;
        let statistics = request::ActiveModel {
            tier: Set(attempt.tier.into()),
            model_used: Set(attempt.model_used),
            used_backup: Set(attempt.used_backup),
            tokens_in: Set(attempt.tokens_in),
            tokens_out: Set(attempt.tokens_out),
            latency_ms: Set(attempt.latency_ms),
            outcome: Set(attempt.outcome),
            finish_reason: Set(attempt.finish_reason.into()),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&both_rows_or_neither)
        .await?;

        payload::ActiveModel {
            request_id: Set(statistics.id),
            sent: Set(attempt.sent),
            received: Set(attempt.received),
            created_at: Set(now),
            expires_at: Set(payload_expiry(now)),
            ..Default::default()
        }
        .insert(&both_rows_or_neither)
        .await?;

        both_rows_or_neither.commit().await
    }

    async fn drop_expired_payloads(&self, now: DateTime<Utc>) -> Result<u64, DbErr> {
        payload::Entity::delete_many()
            .filter(payload::Column::ExpiresAt.lt(now))
            .exec(&self.db)
            .await
            .map(|deleted| deleted.rows_affected)
    }
}

pub fn payload_expiry(now: DateTime<Utc>) -> DateTime<Utc> {
    now + Duration::days(PAYLOAD_RETENTION_DAYS)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::entity::request::{Finish, Tier};
    use crate::test_db;

    fn answered_attempt() -> Attempt {
        Attempt {
            tier: QualityTier::Low,
            model_used: "deepseek/deepseek-v4-flash".to_owned(),
            used_backup: false,
            tokens_in: 10,
            tokens_out: 2,
            latency_ms: 431,
            outcome: Outcome::Answered,
            finish_reason: FinishReason::Stop,
            sent: json!({"model": "deepseek/deepseek-v4-flash", "messages": []}),
            received: json!({"choices": [{"message": {"content": "docs"}}]}),
        }
    }

    #[test]
    fn payloads_are_kept_for_a_month() {
        let now = DateTime::parse_from_rfc3339("2026-09-11T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);

        assert_eq!(
            payload_expiry(now).to_rfc3339(),
            "2026-10-11T00:00:00+00:00"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_call_keeps_both_its_statistics_and_its_payloads() {
        let test = test_db::start().await;
        let log = PgRequestLog::new(test.db.clone());

        log.record(answered_attempt()).await.unwrap();

        let rows = request::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tier, Tier::Low);
        assert_eq!(rows[0].model_used, "deepseek/deepseek-v4-flash");
        assert_eq!(rows[0].tokens_in, 10);
        assert_eq!(rows[0].tokens_out, 2);
        assert_eq!(rows[0].latency_ms, 431);
        assert_eq!(rows[0].outcome, Outcome::Answered);
        assert_eq!(rows[0].finish_reason, Finish::Stop);
        assert!(!rows[0].used_backup);

        let payloads = payload::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].request_id, rows[0].id);
        assert_eq!(
            payloads[0].received["choices"][0]["message"]["content"],
            "docs"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_call_is_recorded_with_what_went_wrong() {
        let test = test_db::start().await;
        let log = PgRequestLog::new(test.db.clone());

        log.record(Attempt {
            outcome: Outcome::Failed,
            tokens_in: 0,
            tokens_out: 0,
            finish_reason: FinishReason::Unspecified,
            received: json!({"error": "the provider answered 503"}),
            ..answered_attempt()
        })
        .await
        .unwrap();

        let rows = request::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows[0].outcome, Outcome::Failed);
        assert_eq!(rows[0].finish_reason, Finish::Unspecified);

        let payloads = payload::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(payloads[0].received["error"], "the provider answered 503");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn expired_payloads_go_and_the_statistics_stay() {
        let test = test_db::start().await;
        let log = PgRequestLog::new(test.db.clone());
        let now = Utc::now();

        log.record(answered_attempt()).await.unwrap();
        log.record(answered_attempt()).await.unwrap();

        let payloads = payload::Entity::find().all(&test.db).await.unwrap();
        let aged: payload::ActiveModel = payload::ActiveModel {
            id: Set(payloads[0].id),
            expires_at: Set(now - Duration::seconds(1)),
            ..Default::default()
        };
        aged.update(&test.db).await.unwrap();

        let removed = log.drop_expired_payloads(now).await.unwrap();

        assert_eq!(removed, 1);
        let left = payload::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, payloads[1].id);
        assert_eq!(
            request::Entity::find().all(&test.db).await.unwrap().len(),
            2
        );
    }
}
