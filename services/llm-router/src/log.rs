// Records what every call cost and how it went, keeping the statistics long and the payloads only as long as a post-mortem needs them.

use chrono::{DateTime, Utc};
use common::proto::llm_router::v1::{FinishReason, QualityTier};
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, DatabaseConnection, DbErr, TransactionTrait};
use serde_json::Value;

use crate::audit::request::Outcome;
use crate::audit::{bounded_model_name, decision, decision_payload, payload, request};
use crate::partition::{self, Swept};

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

pub struct DecisionAttempt {
    pub model_used: String,
    pub questions: i16,
    pub tokens_in: i32,
    pub tokens_out: i32,
    pub latency_ms: i32,
    pub outcome: Outcome,
    pub sent: Value,
    pub received: Value,
}

pub trait RequestLog: Clone + Send + Sync + 'static {
    fn record(&self, attempt: Attempt) -> impl Future<Output = Result<(), DbErr>> + Send;
}

pub trait DecisionLog: Clone + Send + Sync + 'static {
    fn record_decision(
        &self,
        attempt: DecisionAttempt,
    ) -> impl Future<Output = Result<(), DbErr>> + Send;
}

#[derive(Clone)]
pub struct PgAuditLog {
    db: DatabaseConnection,
}

impl PgAuditLog {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    // Retention belongs to the tables rather than to either class of call: one periodic job opens the
    // months ahead for all four audit tables and drops the payload months past retention.
    pub async fn maintain_partitions(&self, now: DateTime<Utc>) -> Result<Swept, DbErr> {
        partition::maintain(&self.db, now).await
    }
}

impl RequestLog for PgAuditLog {
    async fn record(&self, attempt: Attempt) -> Result<(), DbErr> {
        let now = Utc::now();
        let both_rows_or_neither = self.db.begin().await?;
        let statistics = request::ActiveModel {
            tier: Set(attempt.tier.into()),
            model_used: Set(bounded_model_name(attempt.model_used)),
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
            ..Default::default()
        }
        .insert(&both_rows_or_neither)
        .await?;

        both_rows_or_neither.commit().await
    }
}

impl DecisionLog for PgAuditLog {
    async fn record_decision(&self, attempt: DecisionAttempt) -> Result<(), DbErr> {
        let now = Utc::now();
        let both_rows_or_neither = self.db.begin().await?;
        let statistics = decision::ActiveModel {
            model_used: Set(bounded_model_name(attempt.model_used)),
            questions: Set(attempt.questions),
            tokens_in: Set(attempt.tokens_in),
            tokens_out: Set(attempt.tokens_out),
            latency_ms: Set(attempt.latency_ms),
            outcome: Set(attempt.outcome),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&both_rows_or_neither)
        .await?;

        decision_payload::ActiveModel {
            decision_id: Set(statistics.id),
            sent: Set(attempt.sent),
            received: Set(attempt.received),
            created_at: Set(now),
            ..Default::default()
        }
        .insert(&both_rows_or_neither)
        .await?;

        both_rows_or_neither.commit().await
    }
}

#[cfg(test)]
mod tests {
    use sea_orm::EntityTrait;
    use serde_json::json;

    use super::*;
    use crate::audit::MODEL_NAME_BYTES;
    use crate::audit::request::{Finish, Tier};
    use crate::test_db;

    fn decided_attempt() -> DecisionAttempt {
        DecisionAttempt {
            model_used: "jev-1.13.0".to_owned(),
            questions: 3,
            tokens_in: 312,
            tokens_out: 48,
            latency_ms: 274,
            outcome: Outcome::Answered,
            sent: json!({"model": "jev-latest", "questions": []}),
            received: json!({"answers": {"is_urgent": {"type": "noul", "noul": 0.92}}}),
        }
    }

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

    #[tokio::test(flavor = "multi_thread")]
    async fn a_recorded_call_keeps_both_its_statistics_and_its_payloads() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());

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
        let log = PgAuditLog::new(test.db.clone());

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
    async fn a_recorded_decision_keeps_both_its_statistics_and_its_payloads() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());

        log.record_decision(decided_attempt()).await.unwrap();

        let rows = decision::Entity::find().all(&test.db).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model_used, "jev-1.13.0");
        assert_eq!(rows[0].questions, 3);
        assert_eq!(rows[0].tokens_in, 312);
        assert_eq!(rows[0].tokens_out, 48);
        assert_eq!(rows[0].latency_ms, 274);
        assert_eq!(rows[0].outcome, Outcome::Answered);

        let payloads = decision_payload::Entity::find()
            .all(&test.db)
            .await
            .unwrap();
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].decision_id, rows[0].id);
        assert_eq!(payloads[0].sent["model"], "jev-latest");
        assert_eq!(payloads[0].received["answers"]["is_urgent"]["noul"], 0.92);
    }

    // Postgres stores no NUL inside a jsonb string, so this is a payload insert that fails after the
    // statistics row is already in. Both writers have the same begin/insert/insert/commit shape, and both
    // must leave nothing behind, or the call count would drift from the payloads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_decision_whose_payload_will_not_store_leaves_no_statistics_behind() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());

        let refused = log
            .record_decision(DecisionAttempt {
                received: json!({"error": "unreadable \u{0}"}),
                ..decided_attempt()
            })
            .await;

        assert!(refused.is_err(), "{refused:?}");
        assert!(
            decision::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            decision_payload::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_call_whose_payload_will_not_store_leaves_no_statistics_behind() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());

        let refused = log
            .record(Attempt {
                received: json!({"error": "unreadable \u{0}"}),
                ..answered_attempt()
            })
            .await;

        assert!(refused.is_err(), "{refused:?}");
        assert!(
            request::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            payload::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .is_empty()
        );
    }

    // The configured decision model and the tier slugs are deployment config, unbounded until this cut: a
    // name over the column width would fail the statistics insert and take the payload of the call with it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_model_name_longer_than_the_column_still_records_the_call() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());
        let too_long = "é".repeat(MODEL_NAME_BYTES);

        log.record(Attempt {
            model_used: too_long.clone(),
            ..answered_attempt()
        })
        .await
        .unwrap();
        log.record_decision(DecisionAttempt {
            model_used: too_long,
            ..decided_attempt()
        })
        .await
        .unwrap();

        assert_eq!(
            request::Entity::find().all(&test.db).await.unwrap()[0].model_used,
            "é".repeat(MODEL_NAME_BYTES / 2)
        );
        assert_eq!(
            decision::Entity::find().all(&test.db).await.unwrap()[0].model_used,
            "é".repeat(MODEL_NAME_BYTES / 2)
        );
        assert_eq!(
            payload::Entity::find().all(&test.db).await.unwrap().len(),
            1
        );
        assert_eq!(
            decision_payload::Entity::find()
                .all(&test.db)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_fresh_database_has_nothing_to_sweep() {
        let test = test_db::start().await;
        let log = PgAuditLog::new(test.db.clone());

        let swept = log.maintain_partitions(Utc::now()).await.unwrap();

        assert!(swept.is_empty(), "{swept:?}");
    }
}
