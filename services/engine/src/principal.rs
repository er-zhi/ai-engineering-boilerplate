// Reads the two headers Gateway will stamp on every proxied request once it starts fronting
// Engine (Chat Service's plan — Engine doesn't write these, only trusts and reads them; see the
// spec's "Principal и user_id" section for why there's no write side here yet).

use http::HeaderMap;
use uuid::Uuid;

pub const USER_ID_HEADER: &str = "x-principal-user-id";
pub const SESSION_ID_HEADER: &str = "x-principal-session-id";

// Not Copy — session_id is a String, per the spec's `Principal { user_id, session_id }` (the
// spec keeps session_id even though nothing reads it back out of Principal yet, since it's the
// same value Gateway's future interceptor will correlate logs by).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Principal {
    pub user_id: Uuid,
    pub session_id: String,
}

#[must_use]
pub fn from_metadata(headers: &HeaderMap) -> Option<Principal> {
    let user_id = headers.get(USER_ID_HEADER)?.to_str().ok()?;
    let user_id = Uuid::parse_str(user_id).ok()?;
    let session_id = headers.get(SESSION_ID_HEADER)?.to_str().ok()?.to_owned();
    Some(Principal {
        user_id,
        session_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(user_id: &str, session_id: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(USER_ID_HEADER, user_id.parse().expect("valid header value"));
        headers.insert(
            SESSION_ID_HEADER,
            session_id.parse().expect("valid header value"),
        );
        headers
    }

    #[test]
    fn valid_headers_parse_into_a_principal() {
        let id = Uuid::new_v4();
        let principal = from_metadata(&headers(&id.to_string(), "sess-1")).expect("some");
        assert_eq!(principal.user_id, id);
        assert_eq!(principal.session_id, "sess-1");
    }

    #[test]
    fn missing_headers_yield_none() {
        assert_eq!(from_metadata(&HeaderMap::new()), None);
    }

    #[test]
    fn a_malformed_user_id_yields_none() {
        assert_eq!(from_metadata(&headers("not-a-uuid", "sess-1")), None);
    }
}
