// Chat's own lifecycle event and the in-process bus that carries it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

const DEFAULT_CAPACITY: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TopicEventKind {
    TopicCreated,
    TopicQueued,
    TopicStarted,
    TopicProgress,
    TopicCompleted,
    TopicFailed,
    TopicCancelled,
    FocusChanged,
    Notification,
    SessionReset,
    ClarificationNeeded,
}

impl TopicEventKind {
    #[must_use]
    pub fn as_column(self) -> String {
        match serde_json::to_value(self) {
            Ok(serde_json::Value::String(spelling)) => spelling,
            _ => {
                debug_assert!(false, "a TopicEventKind is serialized as a string");
                String::new()
            }
        }
    }

    #[must_use]
    pub fn from_column(kind: &str) -> Option<Self> {
        serde_json::from_value(serde_json::Value::String(kind.to_owned())).ok()
    }
}

impl std::fmt::Display for TopicEventKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.as_column())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopicEvent {
    pub event_id: Uuid,
    pub session_id: Uuid,
    pub topic_id: Option<i64>,
    pub kind: TopicEventKind,
    pub payload: serde_json::Value,
    pub occurred_at: DateTime<Utc>,
}

impl TopicEvent {
    #[must_use]
    pub fn new(
        session_id: Uuid,
        topic_id: Option<i64>,
        kind: TopicEventKind,
        occurred_at: DateTime<Utc>,
    ) -> Self {
        Self {
            event_id: Uuid::new_v4(),
            session_id,
            topic_id,
            kind,
            payload: serde_json::json!({}),
            occurred_at,
        }
    }

    /// A session-level event: nothing it describes belongs to any one topic.
    #[must_use]
    pub fn session_wide(session_id: Uuid, kind: TopicEventKind) -> Self {
        Self::new(session_id, None, kind, Utc::now())
    }

    #[must_use]
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    #[must_use]
    pub fn with_event_id(mut self, event_id: Uuid) -> Self {
        self.event_id = event_id;
        self
    }
}

pub struct EventBus {
    sender: broadcast::Sender<TopicEvent>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl EventBus {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let (sender, _receiver) = broadcast::channel(capacity);
        Self { sender }
    }

    pub fn publish(&self, event: TopicEvent) {
        no_live_subscriber_is_not_an_error(self.sender.send(event));
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<TopicEvent> {
        self.sender.subscribe()
    }
}

fn no_live_subscriber_is_not_an_error(
    sent: Result<usize, broadcast::error::SendError<TopicEvent>>,
) {
    if sent.is_err() {
        tracing::trace!("a chat event was published with no live StreamEvents subscriber");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: TopicEventKind) -> TopicEvent {
        TopicEvent::new(Uuid::new_v4(), Some(1), kind, Utc::now())
    }

    #[test]
    fn a_stored_spelling_this_build_does_not_know_is_not_a_kind() {
        assert_eq!(TopicEventKind::from_column("something_else"), None);
    }

    #[tokio::test]
    async fn a_subscriber_receives_a_published_event() {
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();
        let published = event(TopicEventKind::TopicProgress);

        bus.publish(published.clone());

        let received = receiver.recv().await.expect("recv");
        assert_eq!(received, published);
    }

    #[test]
    fn publishing_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new(8);
        bus.publish(event(TopicEventKind::TopicStarted));
    }

    #[tokio::test]
    async fn an_event_published_after_subscribing_is_still_there_to_read_later() {
        let bus = EventBus::new(8);
        let mut live = bus.subscribe();
        let published = event(TopicEventKind::TopicCompleted);

        bus.publish(published.clone());

        assert_eq!(
            live.try_recv().expect("the event must not have been lost"),
            published
        );
    }
}
