// A key-value store with expiry, backed by gateway.sessions: an expired row is treated as absent everywhere, and a background sweep deletes rows nothing will read again.

use std::time::Duration;

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};

use crate::entity::cache_entry;

#[derive(Debug, PartialEq, Eq)]
pub enum CacheError {
    Unavailable(String),
}

#[derive(Clone)]
pub struct PgCacheStore {
    db: DatabaseConnection,
}

impl PgCacheStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn drop_expired(&self, now: chrono::DateTime<Utc>) -> Result<u64, sea_orm::DbErr> {
        cache_entry::Entity::delete_many()
            .filter(cache_entry::Column::ExpiresAt.lt(now))
            .exec(&self.db)
            .await
            .map(|deleted| deleted.rows_affected)
    }

    pub async fn get(&self, key: &str) -> Result<Option<Vec<u8>>, CacheError> {
        let now = Utc::now();
        cache_entry::Entity::find_by_id(key.to_owned())
            .one(&self.db)
            .await
            .map_err(unavailable)
            .map(|row| row.filter(|row| row.expires_at > now).map(|row| row.value))
    }

    pub async fn set(&self, key: &str, value: &[u8], ttl: Duration) -> Result<(), CacheError> {
        let expires_at = Utc::now()
            + chrono::Duration::from_std(ttl).map_err(|error| unavailable(error.to_string()))?;
        let row = cache_entry::ActiveModel {
            key: Set(key.to_owned()),
            value: Set(value.to_vec()),
            expires_at: Set(expires_at),
        };

        cache_entry::Entity::insert(row)
            .on_conflict(
                OnConflict::column(cache_entry::Column::Key)
                    .update_columns([cache_entry::Column::Value, cache_entry::Column::ExpiresAt])
                    .to_owned(),
            )
            .exec(&self.db)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    pub async fn delete(&self, key: &str) -> Result<(), CacheError> {
        cache_entry::Entity::delete_by_id(key.to_owned())
            .exec(&self.db)
            .await
            .map_err(unavailable)?;
        Ok(())
    }
}

fn unavailable(error: impl std::fmt::Display) -> CacheError {
    CacheError::Unavailable(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_db;

    #[tokio::test(flavor = "multi_thread")]
    async fn a_missing_key_is_none() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        assert_eq!(store.get("missing").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_set_key_round_trips_its_bytes() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        store
            .set("k1", b"hello", Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(store.get("k1").await.unwrap(), Some(b"hello".to_vec()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn setting_the_same_key_again_replaces_the_value_and_ttl() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        store
            .set("k1", b"first", Duration::from_secs(60))
            .await
            .unwrap();
        store
            .set("k1", b"second", Duration::from_secs(60))
            .await
            .unwrap();

        assert_eq!(store.get("k1").await.unwrap(), Some(b"second".to_vec()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_expired_key_reads_as_absent() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        store
            .set("k1", b"hello", Duration::from_millis(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        assert_eq!(store.get("k1").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_key_makes_it_absent() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        store
            .set("k1", b"hello", Duration::from_secs(60))
            .await
            .unwrap();
        store.delete("k1").await.unwrap();

        assert_eq!(store.get("k1").await.unwrap(), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn deleting_a_missing_key_is_not_an_error() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());

        store.delete("missing").await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_expired_rows_leaves_live_ones() {
        let test = test_db::start().await;
        let store = PgCacheStore::new(test.db.clone());
        store
            .set("expired", b"old", Duration::from_millis(1))
            .await
            .unwrap();
        store
            .set("live", b"new", Duration::from_secs(60))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let removed = store.drop_expired(Utc::now()).await.unwrap();

        assert_eq!(removed, 1);
        assert_eq!(store.get("live").await.unwrap(), Some(b"new".to_vec()));
    }
}
