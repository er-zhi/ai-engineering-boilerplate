// The service's error enum and its mapping to Connect error codes.

use connectrpc::ConnectError;

const INTERNAL_FAILURE: &str = "engine could not complete the request";

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("invalid graph: {0}")]
    InvalidGraph(String),
    #[error("graph not found: {0} v{1:?}")]
    GraphNotFound(String, Option<i32>),
    #[error("execution not found: {0}")]
    ExecutionNotFound(uuid::Uuid),
    #[error("execution {0} belongs to another user")]
    NotYours(uuid::Uuid),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// The caller's request was fine; the execution it named is mid-tick and cannot be touched
    /// right now. Distinct from `InvalidRequest`: retrying later, unchanged, can succeed, which is
    /// exactly what `invalid_argument` promises never happens — see the `From` impl below.
    #[error("execution busy: {0}")]
    Busy(String),
    #[error("storage error: {0}")]
    Storage(String),
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
            EngineError::Busy(_) => ConnectError::unavailable(error.to_string()),
            EngineError::NotYours(_) => ConnectError::permission_denied(error.to_string()),
            EngineError::GraphNotFound(..) | EngineError::ExecutionNotFound(_) => {
                ConnectError::not_found(error.to_string())
            }
            EngineError::Storage(_) | EngineError::Db(_) | EngineError::Json(_) => {
                tracing::error!(%error, "engine request failed");
                ConnectError::internal(INTERNAL_FAILURE)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_database_failure_never_carries_schema_text_to_the_caller() {
        let leaky = sea_orm::DbErr::Query(sea_orm::RuntimeErr::Internal(
            "error returned from database: column \"engine.executions.lease_owner\" \
             violates check constraint \"executions_lease_owner_check\""
                .to_owned(),
        ));

        let connect = ConnectError::from(EngineError::Db(leaky));

        let rendered = connect.to_string();
        for leaked in [
            "engine.executions",
            "lease_owner",
            "constraint",
            "column",
            "database",
        ] {
            assert!(!rendered.contains(leaked), "{rendered}");
        }
        assert!(rendered.contains(INTERNAL_FAILURE), "{rendered}");
    }

    #[test]
    fn a_storage_failure_answers_internal_not_invalid_argument() {
        let connect = ConnectError::from(EngineError::Storage(
            "error returned from database: relation \"engine.checkpoints\" does not exist"
                .to_owned(),
        ));

        let rendered = connect.to_string();
        assert!(rendered.contains(INTERNAL_FAILURE), "{rendered}");
        assert!(!rendered.contains("engine.checkpoints"), "{rendered}");
        assert_eq!(connect.code, ConnectError::internal("").code);
    }

    #[test]
    fn a_caller_error_still_says_what_the_caller_got_wrong() {
        let connect = ConnectError::from(EngineError::InvalidRequest(
            "input_json must decode to a JSON object".to_owned(),
        ));

        assert!(
            connect.to_string().contains("must decode to a JSON object"),
            "{connect}"
        );
    }

    // `unavailable`, not `invalid_argument`: a mid-tick execution is not a bad request, and the
    // caller should not be told its retry will fail the same way — see `Busy`'s doc.
    #[test]
    fn a_busy_execution_answers_unavailable_not_invalid_argument() {
        let connect = ConnectError::from(EngineError::Busy(
            "execution 1 is mid-tick, retry shortly".to_owned(),
        ));

        assert_eq!(connect.code, ConnectError::unavailable("").code);
        assert!(connect.to_string().contains("mid-tick"), "{connect}");
    }
}
