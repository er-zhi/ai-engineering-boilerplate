// The status vocabulary: the database column's numbers on one side, the chat.v1 enum on the other.

use buffa::Enumeration;
use common::proto::chat::v1::TopicStatus as TopicStatusProto;

use crate::entity::topic::Status;

const TOPIC_STATUS_NAME_PREFIX: &str = "TOPIC_STATUS_";

#[must_use]
pub fn status_proto(status: Status) -> TopicStatusProto {
    match status {
        Status::Queued => TopicStatusProto::Queued,
        Status::Running => TopicStatusProto::Running,
        Status::Completed => TopicStatusProto::Completed,
        Status::Failed => TopicStatusProto::Failed,
        Status::Cancelled => TopicStatusProto::Cancelled,
    }
}

#[must_use]
pub fn status_word(status: Status) -> String {
    status_proto(status)
        .proto_name()
        .trim_start_matches(TOPIC_STATUS_NAME_PREFIX)
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_names_itself_as_the_contract_spells_it() {
        assert_eq!(status_word(Status::Running), "running");
        assert_eq!(status_word(Status::Cancelled), "cancelled");
    }
}
