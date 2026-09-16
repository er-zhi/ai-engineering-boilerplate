// The only place engine polls anything: LISTEN engine_tick (Task 15's commit sends it after
// every super-step that leaves more work to do) plus a fallback interval, in case a
// notification is ever missed (a reconnect, a brief listener gap) — never a silent stall.
// tokio::sync::Semaphore is the one concurrency limit; no task queue, no actor framework.

use std::sync::Arc;
use std::time::Duration;

use engine_core::TaskExecutor;
use sqlx::postgres::PgListener;
use tokio::sync::Semaphore;

use crate::tick::Tick;

const FALLBACK_INTERVAL: Duration = Duration::from_secs(2);

pub async fn run_forever<E: TaskExecutor + 'static>(
    tick: Arc<Tick<E>>,
    database_url: &str,
    concurrency: usize,
) {
    let semaphore = Arc::new(Semaphore::new(concurrency));
    let mut listener = connect_listener(database_url).await;
    let mut interval = tokio::time::interval(FALLBACK_INTERVAL);

    loop {
        match &mut listener {
            Some(l) => {
                tokio::select! {
                    _ = l.recv() => {}
                    _ = interval.tick() => {}
                }
            }
            None => {
                interval.tick().await;
            }
        }
        try_spawn_one(&tick, &semaphore);
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
