// chat.events: the session's append-only event log, partitioned by month, and its maintenance.

use chrono::{DateTime, Utc};
use common::partition::Monthly;
use sea_orm::entity::prelude::*;

pub use common::partition::month_start;

pub mod event {
    use sea_orm::entity::prelude::*;

    #[sea_orm::model]
    #[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
    #[sea_orm(table_name = "events", schema_name = "chat")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub id: i64,
        pub event_id: Uuid,
        pub session_id: Uuid,
        pub topic_id: Option<i64>,
        #[sea_orm(column_type = "String(StringLen::N(32))")]
        pub kind: String,
        #[sea_orm(column_type = "JsonBinary")]
        pub payload_json: Json,
        pub occurred_at: DateTimeUtc,
    }

    impl ActiveModelBehavior for ActiveModel {}
}

const EVENTS: Monthly = Monthly::new("chat", "events", "occurred_at");
const MONTHS_OPEN_AHEAD: u32 = 1;

pub const TABLE_STATEMENTS: [&str; 3] = [
    "CREATE TABLE IF NOT EXISTS chat.events ( \
     id bigserial NOT NULL, \
     event_id uuid NOT NULL, \
     session_id uuid NOT NULL, \
     topic_id bigint, \
     kind varchar(32) NOT NULL, \
     payload_json jsonb NOT NULL, \
     occurred_at timestamptz NOT NULL, \
     PRIMARY KEY (occurred_at, id) \
     ) PARTITION BY RANGE (occurred_at)",
    "CREATE INDEX IF NOT EXISTS events_session_id_id_idx ON chat.events (session_id, id)",
    "CREATE UNIQUE INDEX IF NOT EXISTS events_occurred_at_event_id_idx \
     ON chat.events (occurred_at, event_id)",
];

pub async fn setup(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    for statement in TABLE_STATEMENTS {
        db.execute_unprepared(statement).await?;
    }
    EVENTS
        .ensure_open_through(db, Utc::now(), MONTHS_OPEN_AHEAD)
        .await
}

pub async fn maintain(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: Option<u32>,
) -> Result<(), DbErr> {
    EVENTS
        .ensure_open_through(db, now, MONTHS_OPEN_AHEAD)
        .await?;
    if let Some(months) = retention_months {
        let dropped = EVENTS.drop_older_than(db, now, months).await?;
        if !dropped.is_empty() {
            tracing::info!(?dropped, "dropped chat.events partitions past retention");
        }
    }
    Ok(())
}

pub async fn is_partitioned(db: &impl ConnectionTrait) -> Result<bool, DbErr> {
    EVENTS.is_partitioned(db).await
}

#[must_use]
pub fn clamp_to_open_window(occurred_at: DateTime<Utc>, now: DateTime<Utc>) -> DateTime<Utc> {
    common::partition::clamp_to_open_window(occurred_at, now, MONTHS_OPEN_AHEAD)
}
