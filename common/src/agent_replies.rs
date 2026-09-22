// The replies the agent writes about itself rather than about what was asked — a decline, or a request for a value the message never gave — declared once because Engine writes them and Chat has to recognise them.

pub const DECLINE_OPENING: &str = "I can only answer from what I'm connected to: ";
pub const CLARIFY_OPENING: &str = "I can look that up, but I need ";
pub const NOTHING_CONNECTED: &str =
    "I'm not connected to anything I can answer from at the moment. Please try again shortly.";
pub const COMPOSE_FAILURE: &str =
    "I couldn't work out what to look up for that. Could you say it again with the name in it?";
pub const PROMISE_REFUSED: &str = "I couldn't get that just now. Ask me again and I'll try.";
pub const UNGROUNDED_REFUSED: &str = "I got something back but couldn't read the answer out of it.";

const EVERY_REPLY_THE_AGENT_WRITES_ABOUT_ITSELF: [&str; 6] = [
    DECLINE_OPENING,
    CLARIFY_OPENING,
    NOTHING_CONNECTED,
    COMPOSE_FAILURE,
    PROMISE_REFUSED,
    UNGROUNDED_REFUSED,
];

#[must_use]
pub fn is_about_itself(reply: &str) -> bool {
    let reply = reply.trim_start();
    EVERY_REPLY_THE_AGENT_WRITES_ABOUT_ITSELF
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
    fn every_reply_the_agent_writes_about_itself_is_recognised() {
        for reply in EVERY_REPLY_THE_AGENT_WRITES_ABOUT_ITSELF {
            assert!(is_about_itself(reply), "not recognised: {reply}");
        }
    }

    #[test]
    fn an_answer_is_not() {
        assert!(!is_about_itself("29.63"));
        assert!(!is_about_itself("Tokyo"));
        assert!(!is_about_itself("Bishkek. 22 degrees, clear."));
    }
}
