// The live topic-lifecycle event bus GetSession's snapshot (Task 3) doesn't cover — a plain
// broadcast channel, one process, no persistence. TopicManager (Task 5) publishes; StreamEvents
// (Task 7) subscribes.

use common::proto::chat::v1::ChatEvent;
use tokio::sync::broadcast;

const DEFAULT_CAPACITY: usize = 256;

/// `StreamEvents`' two halves in the only order that loses nothing: open the live subscription
/// **first**, then read the DB snapshot it will be chained onto.
///
/// The other order leaves a window between the snapshot read and the subscribe in which a
/// published event reaches nobody — `EventBus` is a plain broadcast channel with no history, so
/// an event missed there is missed for good, and a topic could finish without the client ever
/// hearing about it. Subscribing first can instead deliver an event twice (once implied by the
/// snapshot, once live). That is the right way round to be wrong: the client refreshes with
/// `GetSession` on a lifecycle event, so a duplicate is a redundant refresh, while a gap is a
/// permanently stale panel.
pub async fn subscribe_then_snapshot<T, E, Fut>(
    bus: &EventBus,
    snapshot: Fut,
) -> Result<(broadcast::Receiver<ChatEvent>, T), E>
where
    Fut: std::future::Future<Output = Result<T, E>>,
{
    let live = bus.subscribe();
    Ok((live, snapshot.await?))
}

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

    /// The finding: `StreamEvents` read its DB snapshot before subscribing, so anything
    /// published in between vanished. The snapshot future here publishes while it is in flight —
    /// exactly the event that used to be dropped — and it must still arrive on the live stream.
    #[tokio::test]
    async fn an_event_published_during_the_snapshot_read_still_reaches_the_subscriber() {
        let bus = EventBus::new(8);
        let published_mid_read = ChatEvent {
            topic_id: "7".to_owned(),
            kind: "topic_completed".to_owned(),
            ..Default::default()
        };

        let (mut live, snapshot) = subscribe_then_snapshot::<_, (), _>(&bus, async {
            bus.publish(published_mid_read.clone());
            Ok(vec!["the snapshot rows"])
        })
        .await
        .expect("snapshot");

        assert_eq!(snapshot, vec!["the snapshot rows"]);
        assert_eq!(
            live.try_recv().expect("the event must not have been lost"),
            published_mid_read
        );
    }
}
