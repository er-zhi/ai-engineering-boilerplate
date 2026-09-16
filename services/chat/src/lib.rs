// chat as a library: everything main.rs assembles, exposed for unit- and integration-testing
// directly (main.rs stays a thin binary entry point). Each later task adds its own `pub mod`.

pub mod classifier;
pub mod engine_client;
pub mod entity;
pub mod error;
pub mod event_log;
pub mod events;
pub mod session_manager;
pub mod topic_manager;

#[cfg(feature = "test-support")]
pub mod test_db;

/// The wire spelling of a topic status — one definition, shared by the `GetSession` response and
/// by the topic list the classifier is shown, so the two can never drift apart.
#[must_use]
pub fn status_to_str(status: entity::topic::Status) -> &'static str {
    use entity::topic::Status;
    match status {
        Status::Queued => "queued",
        Status::Running => "running",
        Status::Completed => "completed",
        Status::Failed => "failed",
        Status::Cancelled => "cancelled",
    }
}
