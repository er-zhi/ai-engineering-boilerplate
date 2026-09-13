// Holds a Postgres advisory lock so only one crawler instance owns live jobs.

use sea_orm::DatabaseConnection;
use sea_orm::sqlx::pool::PoolConnection;
use sea_orm::sqlx::{Postgres, query_scalar};
use std::io;
use std::time::Duration;

const CRAWLER_INSTANCE_LOCK_ID: i64 = 4_393_489_476_813_169_985;
const OWNERSHIP_CHECK_INTERVAL_MS: u64 = 1_000;
const OWNERSHIP_CHECK_TIMEOUT_MS: u64 = 500;
const TAKEOVER_SAFETY_DELAY_MS: u64 = 2_000;
const OWNERSHIP_CHECK_INTERVAL: Duration = Duration::from_millis(OWNERSHIP_CHECK_INTERVAL_MS);
const OWNERSHIP_CHECK_TIMEOUT: Duration = Duration::from_millis(OWNERSHIP_CHECK_TIMEOUT_MS);
const TAKEOVER_SAFETY_DELAY: Duration = Duration::from_millis(TAKEOVER_SAFETY_DELAY_MS);

const _: () =
    assert!(TAKEOVER_SAFETY_DELAY_MS > OWNERSHIP_CHECK_INTERVAL_MS + OWNERSHIP_CHECK_TIMEOUT_MS);

pub struct CrawlerInstanceGuard {
    _connection: PoolConnection<Postgres>,
}

impl CrawlerInstanceGuard {
    pub async fn acquire(db: &DatabaseConnection) -> Result<Self, Box<dyn std::error::Error>> {
        let mut connection = db.get_postgres_connection_pool().acquire().await?;
        let acquired: bool = query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(CRAWLER_INSTANCE_LOCK_ID)
            .fetch_one(&mut *connection)
            .await?;
        if !acquired {
            return Err("another crawler instance already owns the job queue".into());
        }
        connection.close_on_drop();
        Ok(Self {
            _connection: connection,
        })
    }

    pub async fn wait_for_previous_owner(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::sleep(TAKEOVER_SAFETY_DELAY).await;
        self.check_connection().await
    }

    pub async fn monitor(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        loop {
            tokio::time::sleep(OWNERSHIP_CHECK_INTERVAL).await;
            self.check_connection().await?;
        }
    }

    async fn check_connection(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let heartbeat = query_scalar::<_, i32>("SELECT 1").fetch_one(&mut *self._connection);
        match tokio::time::timeout(OWNERSHIP_CHECK_TIMEOUT, heartbeat).await {
            Ok(result) => result.map(|_| ()).map_err(Into::into),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "crawler ownership heartbeat timed out",
            )
            .into()),
        }
    }

    #[cfg(test)]
    async fn close(self) -> Result<(), sea_orm::sqlx::Error> {
        self._connection.close().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_second_instance_is_rejected_until_the_first_one_stops() {
        let test = crate::test_db::start().await;
        let first = CrawlerInstanceGuard::acquire(&test.db).await.unwrap();

        assert!(CrawlerInstanceGuard::acquire(&test.db).await.is_err());
        first.close().await.unwrap();
        assert!(CrawlerInstanceGuard::acquire(&test.db).await.is_ok());
    }

    #[tokio::test]
    async fn the_owner_can_verify_its_dedicated_connection() {
        let test = crate::test_db::start().await;
        let mut owner = CrawlerInstanceGuard::acquire(&test.db).await.unwrap();

        owner.check_connection().await.unwrap();
    }

    #[tokio::test]
    async fn the_owner_survives_the_takeover_safety_delay() {
        let test = crate::test_db::start().await;
        let mut owner = CrawlerInstanceGuard::acquire(&test.db).await.unwrap();

        owner.wait_for_previous_owner().await.unwrap();
    }
}
