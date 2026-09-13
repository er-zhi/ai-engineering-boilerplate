// A key-value store with expiry, backed by Postgres today. Each service owns its own table and implements this trait against its own schema.

use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
pub enum CacheError {
    Unavailable(String),
}

pub trait CacheStore: Send + Sync {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Vec<u8>>, CacheError>> + Send;
    fn set(
        &self,
        key: &str,
        value: &[u8],
        ttl: Duration,
    ) -> impl Future<Output = Result<(), CacheError>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), CacheError>> + Send;
}
