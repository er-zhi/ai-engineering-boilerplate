// Everything main.rs assembles, exposed as a library so it can be tested directly.

pub mod classifier;
pub mod engine_client;
pub mod entity;
pub mod error;
pub mod event_log;
pub mod events;
pub mod session_manager;
pub mod topic_events;
pub mod topic_focus;
pub mod topic_manager;
pub mod topic_queue;
pub mod topic_status;
pub mod topic_turn;
pub mod topic_watcher;

#[cfg(feature = "test-support")]
pub mod test_db;

#[cfg(all(test, feature = "test-support"))]
mod fakes;
