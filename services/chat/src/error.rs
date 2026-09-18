// One error enum for the service, and the one place that maps it to Connect codes.

use connectrpc::ConnectError;

const INTERNAL_FAILURE_MESSAGE: &str = "chat could not complete the request";

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("topic not found: {0}")]
    TopicNotFound(i64),
    #[error(
        "topic {0} has already finished — create a new topic or choose another one with SetFocus"
    )]
    TopicNotRunning(i64),
    #[error("no focus topic set")]
    NoFocus,
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("engine call failed: {0}")]
    Engine(String),
    /// Engine reported the topic's execution as busy (mid-tick), not broken — see
    /// `services/engine/src/error.rs`'s `EngineError::Busy`. `topic_turn.rs::send_turn` catches
    /// this itself and turns it into a visible event rather than an error, so it should never
    /// reach the `From` impl below in practice; the mapping exists so this stays honest if it ever
    /// does.
    #[error("topic {0} is still busy: {1}")]
    EngineBusy(i64, String),
}

impl From<ChatError> for ConnectError {
    fn from(error: ChatError) -> Self {
        match &error {
            ChatError::InvalidRequest(_) => ConnectError::invalid_argument(error.to_string()),
            ChatError::TopicNotFound(_) => ConnectError::not_found(error.to_string()),
            ChatError::NoFocus | ChatError::TopicNotRunning(_) => {
                ConnectError::failed_precondition(error.to_string())
            }
            ChatError::Db(_) | ChatError::Engine(_) => {
                tracing::error!(%error, "a chat request failed");
                ConnectError::internal(INTERNAL_FAILURE_MESSAGE)
            }
            ChatError::EngineBusy(..) => ConnectError::unavailable(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_internal_failure_never_carries_the_database_detail_to_the_caller() {
        let leaky = "duplicate key value violates unique constraint \"messages_topic_turn\"";
        for error in [
            ChatError::Db(sea_orm::DbErr::Custom(leaky.to_owned())),
            ChatError::Engine(leaky.to_owned()),
        ] {
            let connect: ConnectError = error.into();
            assert_eq!(connect.code, connectrpc::ErrorCode::Internal);
            assert_eq!(connect.message.as_deref(), Some(INTERNAL_FAILURE_MESSAGE));
        }
    }

    #[test]
    fn a_caller_facing_error_still_explains_itself() {
        let connect: ConnectError = ChatError::TopicNotRunning(7).into();
        assert_eq!(connect.code, connectrpc::ErrorCode::FailedPrecondition);
        assert!(connect.message.unwrap_or_default().contains("topic 7"));
    }

    // Defensive, not load-bearing: `send_turn` catches `EngineBusy` itself (see `topic_turn.rs`)
    // before it can ever reach this mapping, but if it ever does escape, it must stay honest —
    // `unavailable`, never `internal`, because retrying really can succeed.
    #[test]
    fn an_engine_busy_error_answers_unavailable() {
        let connect: ConnectError = ChatError::EngineBusy(7, "mid-tick".to_owned()).into();
        assert_eq!(connect.code, connectrpc::ErrorCode::Unavailable);
    }
}
