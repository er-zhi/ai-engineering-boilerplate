// One error enum for the service, one place that maps it to Connect codes — same shape as
// services/engine/src/error.rs.

use connectrpc::ConnectError;

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("tool not found: {0}")]
    NotFound(i64),
    #[error("database error: {0}")]
    Db(#[from] sea_orm::DbErr),
    #[error("invalid json: {0}")]
    Json(#[from] serde_json::Error),
}

impl From<ToolError> for ConnectError {
    fn from(error: ToolError) -> Self {
        match &error {
            ToolError::InvalidRequest(_) => ConnectError::invalid_argument(error.to_string()),
            ToolError::NotFound(_) => ConnectError::not_found(error.to_string()),
            ToolError::Db(_) | ToolError::Json(_) => ConnectError::internal(error.to_string()),
        }
    }
}
