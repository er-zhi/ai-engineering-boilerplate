// One error enum for the whole service, one place that maps it to Connect error codes — the
// spec's decision not to add a common::error module for a single caller (see the spec's "Что
// реально переиспользуется").

use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid graph: {0}")]
    InvalidGraph(String),
    #[error("graph not found: {0} v{1:?}")]
    GraphNotFound(String, Option<i32>),
    #[error("execution not found: {0}")]
    ExecutionNotFound(uuid::Uuid),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<EngineError> for ConnectError {
    fn from(error: EngineError) -> Self {
        match &error {
            EngineError::InvalidGraph(_) | EngineError::InvalidRequest(_) => {
                ConnectError::invalid_argument(error.to_string())
            }
            EngineError::GraphNotFound(..) | EngineError::ExecutionNotFound(_) => {
                ConnectError::not_found(error.to_string())
            }
            EngineError::Db(_) | EngineError::Json(_) => ConnectError::internal(error.to_string()),
        }
    }
}
