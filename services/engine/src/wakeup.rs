// The only place engine polls anything: LISTEN engine_tick (Task 15's commit sends it after
// every super-step that leaves more work to do) plus a fallback interval, in case a
// notification is ever missed (a reconnect, a brief listener gap) — never a silent stall.
// tokio::sync::Semaphore is the one concurrency limit; no task queue, no actor framework.
//
// The fallback interval carries a second job: the service's occasional maintenance — sweeping
// terminal executions out of the hot tables, and keeping execution_events' month partitions
// ahead of the clock. Both hang off the interval arm and never off a NOTIFY, so a busy engine
// (which is exactly when NOTIFY fires constantly) does not pay for them per commit.

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
/// Bounded so one sweep never holds a long transaction next to the claim query.
const SWEEP_BATCH: u32 = 500;

/// What the fallback-interval arm needs for its maintenance half; the tick half needs none of it.
#[derive(Clone)]
pub struct Maintenance {
    pub db: DatabaseConnection,
    /// How long a terminal execution's row survives before the sweep takes it.
    pub terminal_retention: Duration,
    /// `None` — the default — keeps every `execution_events` partition forever.
    pub events_retention_months: Option<u32>,
}

pub async fn run_forever<E: TaskExecutor + 'static>(
    tick: Arc<Tick<E>>,
    database_url: &str,
    concurrency: usize,
    maintenance: Maintenance,
) {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    // One permit for every maintenance job together: the interval fires far more often than a
    // sweep can take, and two overlapping passes would contend for exactly the same rows.
    let maintenance_slot = Arc::new(Semaphore::new(1));
    let mut listener = connect_listener(database_url).await;
    let mut interval = tokio::time::interval(FALLBACK_INTERVAL);
    // Startup already ran one maintenance pass (main.rs), so the first one here is a day out.
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

/// Opens a `LISTEN engine_tick` subscription, or `None` if either the connection or the
/// `LISTEN` itself fails — the caller falls back to polling on `FALLBACK_INTERVAL` alone rather
/// than treating either failure as fatal.
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

/// Spawns at most one `run_one()` per wakeup. If the semaphore is already at capacity this does
/// nothing — a busy tick sends its own `NOTIFY` when it finishes and frees a permit, and the
/// fallback interval covers the case where that, too, is somehow missed, so nothing needs an
/// inner retry loop here.
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

/// Outside the tick semaphore, like every maintenance job here: a sweep is not a tick, and a
/// failed one is a warning, not a failed tick. Skipped outright while another maintenance pass
/// still holds the slot — the next interval tick is two seconds away.
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

/// Once a day: open the month ahead, and drop what fell out of `ENGINE_EVENTS_RETENTION_MONTHS`.
/// Losing one day's pass to a busy slot is harmless — partitions are always a month ahead.
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
