// The few replies the agent writes about itself rather than about what was asked: it declines
// because nothing it is connected to covers the question, or it asks for a value the message never
// gave it.
//
// Engine writes them; Chat has to recognise them. A topic that ends in one found nothing, and
// feeding its text back as conversation is how a reference gets resolved against a refusal —
// measured: "weather there?" became "weather in the current weather for a place", having resolved
// "there" against the wording of a decline. Both services need the same list, so it is declared
// once, here, rather than compared as loose strings in two places.

/// How a decline opens. The rest of it is the catalog's own titles, which differ per deployment.
pub const DECLINE_OPENING: &str = "I can only answer from what I'm connected to: ";
/// How a request for a missing argument opens. The rest comes from the tool's schema.
pub const CLARIFY_OPENING: &str = "I can look that up, but I need ";

/// Whether this reply is the agent talking about itself rather than answering. Such a reply is
/// never context for anything: it contains our words, not the conversation's.
#[must_use]
pub fn is_about_itself(reply: &str) -> bool {
    let reply = reply.trim_start();
    [DECLINE_OPENING, CLARIFY_OPENING]
        .iter()
        .any(|opening| reply.starts_with(opening))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_decline_is_the_agent_talking_about_itself() {
        assert!(is_about_itself(&format!(
            "{DECLINE_OPENING}a source, and another."
        )));
    }

    #[test]
    fn a_request_for_a_missing_value_is_too() {
        assert!(is_about_itself(&format!("{CLARIFY_OPENING}the place.")));
    }

    #[test]
    fn an_answer_is_not() {
        assert!(!is_about_itself("29.63"));
        assert!(!is_about_itself("Tokyo"));
    }
}
