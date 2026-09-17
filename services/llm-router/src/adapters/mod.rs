// One module per provider integration; what every one of them needs — a key check, a bounded read of the
// reply, and one classification of the status a provider answered with — lives here.

pub mod openai_compatible;
pub mod system_one;

use serde_json::Value;

use crate::provider::CallError;

pub const MAX_PROVIDER_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const REQUEST_TIMEOUT: u16 = 408;
const TOO_MANY_REQUESTS: u16 = 429;
const FIRST_SERVER_FAULT: u16 = 500;
// The 4xx that describe the body the caller wrote, as against the ones that describe this deployment. A 401,
// a 402 out of credits, a 403 or a 404 from a mistyped base URL says nothing about the request that carried it.
const BODY_SHAPED_FAULTS: [u16; 4] = [400, 409, 413, 422];

#[derive(Debug, PartialEq)]
pub enum KeyCheck {
    Rejected,
    Unreachable(String),
}

// How a provider's 4xx is reported. The completion path sends a prompt this service assembled, so it has no
// caller-authored body to hand back and every client fault there is ours; the decision path sends the
// caller's own state and questions, so the body-shaped faults are the caller's to fix.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ClientFaults {
    AlwaysFinal,
    RefusedWhenBodyShaped,
}

// A key the provider actively refuses is a deployment mistake and stops startup: a 403, or a 404 from a
// mistyped base URL, is no more usable than a 401. A 408 or a 429 is not a credential and a 5xx is not an
// answer about one, so those warn and let the service come up rather than crash-looping on a rate limiter.
pub fn key_accepted(status: reqwest::StatusCode) -> Result<(), KeyCheck> {
    let code = status.as_u16();
    if code == REQUEST_TIMEOUT || code == TOO_MANY_REQUESTS || code >= FIRST_SERVER_FAULT {
        return Err(KeyCheck::Unreachable(format!(
            "the provider answered {code} on the key probe"
        )));
    }
    if status.is_client_error() {
        return Err(KeyCheck::Rejected);
    }
    Ok(())
}

pub fn error_for_status(status: u16, detail: Option<String>, faults: ClientFaults) -> CallError {
    let message = match detail {
        Some(detail) => format!("the provider answered {status}: {detail}"),
        None => format!("the provider answered {status}"),
    };
    if status == REQUEST_TIMEOUT || status == TOO_MANY_REQUESTS || status >= FIRST_SERVER_FAULT {
        return CallError::WorthRetrying(message);
    }
    if faults == ClientFaults::RefusedWhenBodyShaped && BODY_SHAPED_FAULTS.contains(&status) {
        return CallError::Refused(message);
    }
    CallError::Final(message)
}

pub fn unreachable_provider(error: reqwest::Error) -> CallError {
    CallError::WorthRetrying(if error.is_timeout() {
        "the provider did not answer in time".to_owned()
    } else {
        format!("could not reach the provider: {error}")
    })
}

// A token count that will not read as an int32 becomes 0, the only value the column can carry for "unknown",
// so it is logged: an unexplained zero in the statistics can then be traced back to the reply behind it.
pub fn as_count(field: &str, value: &Value) -> i32 {
    match value.as_i64().and_then(|count| i32::try_from(count).ok()) {
        Some(count) => count,
        None => {
            tracing::warn!(
                field,
                reported = %value,
                "the provider's token count is not a readable int32; recording 0"
            );
            0
        }
    }
}

pub async fn read_provider_response(mut response: reqwest::Response) -> Result<Value, CallError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES as u64)
    {
        return Err(provider_response_too_large());
    }

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        CallError::WorthRetrying(format!("the provider sent a reply we cannot read: {error}"))
    })? {
        if chunk.len() > MAX_PROVIDER_RESPONSE_BYTES.saturating_sub(bytes.len()) {
            return Err(provider_response_too_large());
        }
        bytes.extend_from_slice(&chunk);
    }

    serde_json::from_slice(&bytes).map_err(|error| {
        CallError::WorthRetrying(format!("the provider sent a reply we cannot read: {error}"))
    })
}

fn provider_response_too_large() -> CallError {
    CallError::WorthRetrying(format!(
        "the provider response is larger than {MAX_PROVIDER_RESPONSE_BYTES} bytes"
    ))
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_rate_limit_or_a_server_fault_on_the_key_probe_lets_the_service_start() {
        for status in [408, 429, 500, 502, 503] {
            let checked = key_accepted(StatusCode::from_u16(status).unwrap());

            assert!(
                matches!(checked, Err(KeyCheck::Unreachable(_))),
                "{status} on the key probe is not an answer about the credential: {checked:?}"
            );
        }
    }

    #[test]
    fn a_key_the_provider_refuses_stops_the_service_from_starting() {
        for status in [400, 401, 402, 403, 404] {
            assert_eq!(
                key_accepted(StatusCode::from_u16(status).unwrap()),
                Err(KeyCheck::Rejected),
                "{status} on the key probe is a key or a base URL this deployment cannot use"
            );
        }
    }

    #[test]
    fn a_key_the_provider_accepts_lets_the_service_start() {
        key_accepted(StatusCode::OK).unwrap();
    }

    #[test]
    fn a_timeout_a_rate_limit_and_every_server_fault_are_worth_retrying() {
        for status in [408, 429, 500, 502, 529] {
            for faults in [
                ClientFaults::AlwaysFinal,
                ClientFaults::RefusedWhenBodyShaped,
            ] {
                assert!(
                    matches!(
                        error_for_status(status, None, faults),
                        CallError::WorthRetrying(_)
                    ),
                    "status {status} should be retried"
                );
            }
        }
    }

    #[test]
    fn only_a_body_shaped_fault_is_ever_the_callers_to_fix() {
        for status in [400, 409, 413, 422] {
            assert!(
                matches!(
                    error_for_status(status, None, ClientFaults::RefusedWhenBodyShaped),
                    CallError::Refused(_)
                ),
                "status {status} is the caller's own request coming back"
            );
            assert!(
                matches!(
                    error_for_status(status, None, ClientFaults::AlwaysFinal),
                    CallError::Final(_)
                ),
                "status {status} carries nothing of the caller's on the completion path"
            );
        }
    }

    #[test]
    fn a_key_a_balance_or_a_base_url_is_never_reported_as_the_callers_request() {
        for status in [401, 402, 403, 404, 405, 451] {
            for faults in [
                ClientFaults::AlwaysFinal,
                ClientFaults::RefusedWhenBodyShaped,
            ] {
                assert!(
                    matches!(error_for_status(status, None, faults), CallError::Final(_)),
                    "status {status} is this deployment's own mistake, not the caller's"
                );
            }
        }
    }

    #[test]
    fn a_count_the_provider_did_not_give_is_recorded_as_zero() {
        assert_eq!(as_count("input_tokens", &json!(312)), 312);
        assert_eq!(as_count("input_tokens", &json!(null)), 0);
        assert_eq!(as_count("input_tokens", &json!("312")), 0);
        assert_eq!(as_count("input_tokens", &json!(i64::from(i32::MAX) + 1)), 0);
    }
}
