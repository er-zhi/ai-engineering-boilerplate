// Waits on LISTEN engine_tick and spawns ticks and maintenance passes.

use std::sync::Arc;
use std::time::Duration;

use engine_core::TaskExecutor;
use sea_orm::DatabaseConnection;
use sqlx::postgres::PgListener;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::tick::Tick;
use crate::{partition, sweep};

const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_BATCH: u32 = 500;

#[derive(Clone)]
pub struct Maintenance {
    pub db: DatabaseConnection,
    pub terminal_retention: Duration,
    pub events_retention_months: Option<u32>,
}

pub async fn run_forever<E: TaskExecutor + 'static>(
    tick: Arc<Tick<E>>,
    database_url: &str,
    concurrency: usize,
    maintenance: Maintenance,
) {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let maintenance_slot = Arc::new(Semaphore::new(1));
    let mut listener = connect_listener(database_url).await;
    let mut interval = tokio::time::interval(FALLBACK_INTERVAL);
    let mut next_maintenance = Instant::now() + MAINTENANCE_INTERVAL;

    loop {
        let woke_on_interval = match &mut listener {
            Some(l) => {
                tokio::select! {
                    _ = l.recv() => false,
                    _ = interval.tick() => true,
                }
            }
            None => {
                interval.tick().await;
                true
            }
        };
        try_spawn_one(&tick, &semaphore);
        if woke_on_interval {
            spawn_sweep(&maintenance, &maintenance_slot);
            if Instant::now() >= next_maintenance {
                next_maintenance = Instant::now() + MAINTENANCE_INTERVAL;
                spawn_partition_maintenance(&maintenance, &maintenance_slot);
            }
        }
    }
}

async fn connect_listener(database_url: &str) -> Option<PgListener> {
    let mut listener = match PgListener::connect(database_url).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(%error, "could not open a NOTIFY listener connection, relying on the fallback interval only");
            return None;
        }
    };
    match listener.listen("engine_tick").await {
        Ok(()) => Some(listener),
        Err(error) => {
            tracing::error!(%error, "LISTEN engine_tick failed, relying on the fallback interval only");
            None
        }
    }
}

fn try_spawn_one<E: TaskExecutor + 'static>(tick: &Arc<Tick<E>>, semaphore: &Arc<Semaphore>) {
    let Ok(permit) = Arc::clone(semaphore).try_acquire_owned() else {
        return;
    };
    let tick = Arc::clone(tick);
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = tick.run_one().await {
            tracing::error!(%error, "tick failed");
        }
    });
}

fn spawn_sweep(maintenance: &Maintenance, slot: &Arc<Semaphore>) {
    let Ok(permit) = Arc::clone(slot).try_acquire_owned() else {
        return;
    };
    let maintenance = maintenance.clone();
    tokio::spawn(async move {
        let _permit = permit;
        match sweep::sweep_terminal(&maintenance.db, maintenance.terminal_retention, SWEEP_BATCH)
            .await
        {
            Ok(0) => {}
            Ok(swept) => tracing::info!(swept, "swept terminal executions out of the hot tables"),
            Err(error) => tracing::warn!(%error, "sweeping terminal executions failed"),
        }
    });
}

fn spawn_partition_maintenance(maintenance: &Maintenance, slot: &Arc<Semaphore>) {
    let Ok(permit) = Arc::clone(slot).try_acquire_owned() else {
        return;
    };
    let maintenance = maintenance.clone();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = partition::maintain(
            &maintenance.db,
            chrono::Utc::now(),
            maintenance.events_retention_months,
        )
        .await
        {
            tracing::warn!(%error, "execution_events partition maintenance failed");
        }
    });
}
