// The service's error enum and its one mapping to Connect codes.

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
    /// A `Decide` call that failed for a reason that is ours or the vendor's, not the caller's —
    /// a transient fault, a deployment-side problem, an unreachable decider. Kept distinct from
    /// `InvalidRequest` so a submitter is never told to fix a definition that was never actually
    /// reviewed: see `services/tool/src/service.rs`'s `decide_call_error`.
    #[error("tool review is unavailable: {0}")]
    ReviewUnavailable(String),
}

pub const INTERNAL_MESSAGE_WITHOUT_STORAGE_DETAIL: &str = "the request could not be completed";

impl From<ToolError> for ConnectError {
    fn from(error: ToolError) -> Self {
        match &error {
            ToolError::InvalidRequest(_) => ConnectError::invalid_argument(error.to_string()),
            ToolError::NotFound(_) => ConnectError::not_found(error.to_string()),
            ToolError::Db(_) | ToolError::Json(_) => {
                tracing::error!(%error, "tool service internal error");
                ConnectError::internal(INTERNAL_MESSAGE_WITHOUT_STORAGE_DETAIL)
            }
            ToolError::ReviewUnavailable(_) => ConnectError::unavailable(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_error_never_reaches_the_caller() {
        let leaky = r#"column "slug" of relation "tool.tools" violates constraint"#;

        let connect = ConnectError::from(ToolError::Db(sea_orm::DbErr::Custom(leaky.to_owned())));

        let rendered = format!("{connect:?}");
        assert!(!rendered.contains("tool.tools"), "{rendered}");
        assert!(
            rendered.contains(INTERNAL_MESSAGE_WITHOUT_STORAGE_DETAIL),
            "{rendered}"
        );
    }
}
