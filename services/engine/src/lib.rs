// engine as a library: everything main.rs assembles, exposed so it can be unit- and
// integration-tested directly (main.rs stays a thin binary entry point).

pub mod cron_next;
pub mod dispatch;
pub mod entity;
pub mod error;
pub mod executors;
pub mod lease;
pub mod principal;
pub mod schedule_service;
pub mod scheduler;
pub mod service;
pub mod store;
pub mod stream;
#[cfg(feature = "test-support")]
pub mod test_db;
pub mod tick;
pub mod wakeup;
pub mod wire;
