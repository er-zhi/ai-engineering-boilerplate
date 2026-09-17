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
}
