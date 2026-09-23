// Creates and drops engine.execution_events' monthly partitions.

use chrono::{DateTime, Utc};
use common::partition::Monthly;
use sea_orm::{ConnectionTrait, DbErr};

const EVENTS: Monthly = Monthly::new("engine", "execution_events", "occurred_at");
const MONTHS_OPEN_AHEAD: u32 = 1;

pub async fn ensure_current_and_next(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
) -> Result<(), DbErr> {
    EVENTS.ensure_open_through(db, now, MONTHS_OPEN_AHEAD).await
}

pub async fn ensure_partition(
    db: &impl ConnectionTrait,
    month: DateTime<Utc>,
) -> Result<(), DbErr> {
    EVENTS.ensure(db, month).await
}

pub async fn maintain(
    db: &impl ConnectionTrait,
    now: DateTime<Utc>,
    retention_months: Option<u32>,
) -> Result<(), DbErr> {
    ensure_current_and_next(db, now).await?;
    if let Some(months) = retention_months {
        let dropped = EVENTS.drop_older_than(db, now, months).await?;
        if !dropped.is_empty() {
            tracing::info!(
                ?dropped,
                "dropped execution_events partitions past retention"
            );
        }
    }
    Ok(())
}

pub async fn is_partitioned(db: &impl ConnectionTrait) -> Result<bool, DbErr> {
    EVENTS.is_partitioned(db).await
}
