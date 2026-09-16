// One error enum for the service, one place that maps it to Connect codes — same shape as
// services/tool/src/error.rs.

use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("topic not found: {0}")]
    TopicNotFound(i64),
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
            ChatError::NoFocus => ConnectError::failed_precondition(error.to_string()),
            ChatError::Db(_) | ChatError::Engine(_) => ConnectError::internal(error.to_string()),
        }
    }
}
