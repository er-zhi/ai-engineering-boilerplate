// Session tokens: generated, hashed for storage, and compared to the configured password without leaking timing.

use axum::http::{HeaderMap, header};
use rand::RngCore;
use sha2::{Digest, Sha256};

const TOKEN_BYTES: usize = 32;
pub const COOKIE_NAME: &str = "session";

pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    rand::rng().fill_bytes(&mut bytes);
    to_hex(&bytes)
}

pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    to_hex(&hasher.finalize())
}

pub fn passwords_match(candidate: &str, expected: &str) -> bool {
    let candidate_digest = Sha256::digest(candidate.as_bytes());
    let expected_digest = Sha256::digest(expected.as_bytes());
    let mut difference = 0u8;
    for (a, b) in candidate_digest.iter().zip(expected_digest.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|cookies| cookie_value(cookies, COOKIE_NAME))
}

pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').find_map(|pair| {
        let (key, value) = pair.trim().split_once('=')?;
        (key == name).then_some(value)
    })
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_high_entropy_hex_and_never_repeat() {
        let a = generate_token();
        let b = generate_token();

        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn hashing_the_same_token_twice_gives_the_same_hash() {
        let token = generate_token();

        assert_eq!(hash_token(&token), hash_token(&token));
    }

    #[test]
    fn different_tokens_hash_differently() {
        assert_ne!(hash_token("a"), hash_token("b"));
    }

    #[test]
    fn a_hash_never_reveals_the_token_it_came_from() {
        let token = "correct horse battery staple";

        assert_ne!(hash_token(token), token);
    }

    #[test]
    fn the_matching_password_is_accepted() {
        assert!(passwords_match("hunter2", "hunter2"));
    }

    #[test]
    fn a_wrong_password_is_refused() {
        assert!(!passwords_match("wrong", "hunter2"));
    }

    #[test]
    fn passwords_of_different_lengths_are_still_compared_correctly() {
        assert!(!passwords_match("short", "a-much-longer-password"));
        assert!(passwords_match("same-length-a", "same-length-a"));
    }

    #[test]
    fn cookie_value_finds_the_named_cookie_among_others() {
        let header = "theme=dark; session=abc123; lang=en";

        assert_eq!(cookie_value(header, "session"), Some("abc123"));
    }

    #[test]
    fn cookie_value_is_none_when_the_cookie_is_absent() {
        assert_eq!(cookie_value("theme=dark", "session"), None);
    }

    #[test]
    fn cookie_value_handles_an_empty_header() {
        assert_eq!(cookie_value("", "session"), None);
    }
}
