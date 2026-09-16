// The live topic-lifecycle event bus GetSession's snapshot (Task 3) doesn't cover — a plain
// broadcast channel, one process, no persistence. TopicManager (Task 5) publishes; StreamEvents
// (Task 7) subscribes.

use common::proto::chat::v1::ChatEvent;
use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

pub struct EventBus {
    sender: broadcast::Sender<ChatEvent>,
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

    /// No subscribers is not an error — publishing when nobody's listening is normal (e.g. no
    /// chat frontend is currently connected).
    pub fn publish(&self, event: ChatEvent) {
        let _ = self.sender.send(event);
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<ChatEvent> {
        self.sender.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_subscriber_receives_a_published_event() {
        let bus = EventBus::new(8);
        let mut receiver = bus.subscribe();
        let event = ChatEvent {
            topic_id: "1".to_owned(),
            kind: "topic_progress".to_owned(),
            ..Default::default()
        };

        bus.publish(event.clone());

        let received = receiver.recv().await.expect("recv");
        assert_eq!(received, event);
    }

    #[test]
    fn publishing_with_no_subscribers_does_not_panic() {
        let bus = EventBus::new(8);
        bus.publish(ChatEvent::default());
    }
}
