//! Cross-node control plane for immediate login-session revocation.
//!
//! Database-backed access checks remain authoritative. This ephemeral broadcast
//! path narrows the window between a revocation committed on one node and local
//! `WebSockets` on another node observing it through their periodic DB watchdog.

use std::sync::Arc;

use aero_bus::{EventBus, Subscription};
use aero_common::metrics::{self, names};
use aero_common::{ParticipantId, SessionId};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::hub::Hub;
use crate::state::AppState;

/// Broadcast subject carried by the `IM_EVENTS` `JetStream` stream.
pub const SESSION_CONTROL_SUBJECT: &str = "im.events.session.control";

const RESUBSCRIBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

/// One idempotently-published revocation command.
///
/// `event_id` is minted once with the command and is also used as the NATS
/// message-id, so an ambiguous publish retry of the same command is deduplicated
/// by `JetStream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionControlEvent {
    RevokeSession {
        event_id: uuid::Uuid,
        participant: ParticipantId,
        session: SessionId,
    },
    RevokeParticipant {
        event_id: uuid::Uuid,
        participant: ParticipantId,
    },
    RevokeOthers {
        event_id: uuid::Uuid,
        participant: ParticipantId,
        keep_session: SessionId,
    },
}

impl SessionControlEvent {
    #[must_use]
    pub fn revoke_session(participant: ParticipantId, session: SessionId) -> Self {
        Self::RevokeSession {
            event_id: uuid::Uuid::new_v4(),
            participant,
            session,
        }
    }

    #[must_use]
    pub fn revoke_participant(participant: ParticipantId) -> Self {
        Self::RevokeParticipant {
            event_id: uuid::Uuid::new_v4(),
            participant,
        }
    }

    #[must_use]
    pub fn revoke_others(participant: ParticipantId, keep_session: SessionId) -> Self {
        Self::RevokeOthers {
            event_id: uuid::Uuid::new_v4(),
            participant,
            keep_session,
        }
    }

    #[must_use]
    pub const fn event_id(&self) -> uuid::Uuid {
        match self {
            Self::RevokeSession { event_id, .. }
            | Self::RevokeParticipant { event_id, .. }
            | Self::RevokeOthers { event_id, .. } => *event_id,
        }
    }
}

/// Apply one command to this process's connection registry.
///
/// Duplicate delivery is harmless: cancelling a `CancellationToken` repeatedly
/// is idempotent.
pub fn apply_local(hub: &Hub, event: &SessionControlEvent) -> usize {
    match *event {
        SessionControlEvent::RevokeSession {
            participant,
            session,
            ..
        } => hub.disconnect_session(participant, session),
        SessionControlEvent::RevokeParticipant { participant, .. } => {
            hub.disconnect_participant(participant)
        }
        SessionControlEvent::RevokeOthers {
            participant,
            keep_session,
            ..
        } => hub.disconnect_other_sessions(participant, keep_session),
    }
}

/// Publish one already-minted command. Failure is best-effort because the
/// database mutation has already committed and each sid-bearing WebSocket has a
/// periodic authoritative DB check as a bounded fallback.
pub async fn publish_event(state: &AppState, event: &SessionControlEvent) {
    publish_to_bus(state.bus.as_ref(), event).await;
}

pub async fn publish_revoke_session(
    state: &AppState,
    participant: ParticipantId,
    session: SessionId,
) {
    publish_event(
        state,
        &SessionControlEvent::revoke_session(participant, session),
    )
    .await;
}

pub async fn publish_revoke_participant(state: &AppState, participant: ParticipantId) {
    publish_event(state, &SessionControlEvent::revoke_participant(participant)).await;
}

pub async fn publish_revoke_others(
    state: &AppState,
    participant: ParticipantId,
    keep_session: SessionId,
) {
    publish_event(
        state,
        &SessionControlEvent::revoke_others(participant, keep_session),
    )
    .await;
}

async fn publish_to_bus(bus: &dyn EventBus, event: &SessionControlEvent) {
    let payload = match serde_json::to_vec(event) {
        Ok(payload) => payload,
        Err(error) => {
            warn!(?error, "session-control serialization failed");
            return;
        }
    };
    let event_id = event.event_id().to_string();
    if let Err(error) = bus
        .publish_idempotent(SESSION_CONTROL_SUBJECT, payload.into(), &event_id)
        .await
    {
        warn!(
            ?error,
            %event_id,
            subject = SESSION_CONTROL_SUBJECT,
            "session-control publish failed; WebSocket DB watchdog remains active"
        );
    }
}

/// Run the per-instance ephemeral revocation listener until cancellation.
///
/// Passing `durable = None` makes the bus construct a `DeliverPolicy::New`
/// consumer. Every server instance therefore receives new control events for its
/// own local sockets; retained historical commands are unnecessary because the
/// DB session row is authoritative on connect and on the periodic watchdog.
pub async fn run_listener(state: AppState, cancel: CancellationToken) -> anyhow::Result<()> {
    run_with(state.bus.clone(), state.hub.clone(), cancel).await
}

async fn run_with(
    bus: Arc<dyn EventBus>,
    hub: Arc<Hub>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    loop {
        let subscribed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            result = bus.subscribe(SESSION_CONTROL_SUBJECT, None) => result,
        };
        let mut stream = match subscribed {
            Ok(stream) => stream,
            Err(error) => {
                warn!(?error, "session-control subscribe failed; retrying");
                if backoff_or_cancelled(&cancel).await {
                    return Ok(());
                }
                continue;
            }
        };
        info!("session-control listener started");

        loop {
            let next = tokio::select! {
                biased;
                item = stream.next() => item,
                () = cancel.cancelled() => return Ok(()),
            };
            let Some(subscription) = next else {
                break;
            };
            // Once received, finish applying and ACKing even if cancellation
            // becomes ready concurrently.
            handle_subscription(&hub, subscription).await;
        }

        if cancel.is_cancelled() {
            return Ok(());
        }
        warn!("session-control subscription ended; resubscribing");
        if backoff_or_cancelled(&cancel).await {
            return Ok(());
        }
    }
}

async fn backoff_or_cancelled(cancel: &CancellationToken) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => true,
        () = tokio::time::sleep(RESUBSCRIBE_BACKOFF) => false,
    }
}

async fn handle_subscription(hub: &Hub, sub: Box<dyn Subscription + Send>) {
    match serde_json::from_slice::<SessionControlEvent>(sub.payload()) {
        Ok(event) => {
            let closed = apply_local(hub, &event);
            debug!(
                event_id = %event.event_id(),
                closed,
                "session-control event applied locally"
            );
            ack_or_warn(sub, "session_control_applied").await;
        }
        Err(error) => {
            warn!(
                ?error,
                subject = %sub.subject(),
                "bad session-control payload -- dropping (poison)"
            );
            metrics::inc_counter(names::BUS_POISON_DROPPED_TOTAL, 1);
            ack_or_warn(sub, "session_control_poison_dropped").await;
        }
    }
}

async fn ack_or_warn(sub: Box<dyn Subscription + Send>, disposition: &'static str) {
    if let Err(error) = sub.ack().await {
        warn!(
            ?error,
            subject = %sub.subject(),
            disposition,
            "session-control ACK failed; broker may redeliver"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use aero_bus::traits::{BusError, BusResult, EventBus};
    use async_trait::async_trait;
    use futures::stream::BoxStream;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::hub::WsSender;

    #[test]
    fn every_event_round_trips_with_its_stable_id() {
        let participant = ParticipantId::new();
        let session = SessionId::new();
        let events = [
            SessionControlEvent::revoke_session(participant, session),
            SessionControlEvent::revoke_participant(participant),
            SessionControlEvent::revoke_others(participant, session),
        ];

        for event in events {
            let event_id = event.event_id();
            let payload = serde_json::to_vec(&event).expect("serialize event");
            let decoded: SessionControlEvent =
                serde_json::from_slice(&payload).expect("deserialize event");
            assert_eq!(decoded, event);
            assert_eq!(decoded.event_id(), event_id);
        }
    }

    fn session_sender(session: Option<SessionId>) -> (WsSender, CancellationToken) {
        let (tx, _rx) = mpsc::channel(4);
        let close = CancellationToken::new();
        let sender = match session {
            Some(session) => WsSender::for_session(tx, close.clone(), session),
            None => WsSender::new(tx, close.clone()),
        };
        (sender, close)
    }

    #[test]
    fn local_apply_targets_only_the_requested_sessions() {
        let hub = Hub::default();
        let participant = ParticipantId::new();
        let other_participant = ParticipantId::new();
        let revoked_session = SessionId::new();
        let kept_session = SessionId::new();

        let (tab_a, close_a) = session_sender(Some(revoked_session));
        let (tab_b, close_b) = session_sender(Some(revoked_session));
        let (kept, close_kept) = session_sender(Some(kept_session));
        let (legacy, close_legacy) = session_sender(None);
        let (other, close_other) = session_sender(Some(revoked_session));
        hub.register(participant, tab_a);
        hub.register(participant, tab_b);
        hub.register(participant, kept);
        hub.register(participant, legacy);
        hub.register(other_participant, other);

        let exact = SessionControlEvent::revoke_session(participant, revoked_session);
        assert_eq!(apply_local(&hub, &exact), 2);
        assert!(close_a.is_cancelled());
        assert!(close_b.is_cancelled());
        assert!(!close_kept.is_cancelled());
        assert!(!close_legacy.is_cancelled());
        assert!(!close_other.is_cancelled());

        let others = SessionControlEvent::revoke_others(participant, kept_session);
        assert_eq!(apply_local(&hub, &others), 3);
        assert!(!close_kept.is_cancelled());
        assert!(close_legacy.is_cancelled());
        assert!(!close_other.is_cancelled());

        let all = SessionControlEvent::revoke_participant(participant);
        assert_eq!(apply_local(&hub, &all), 4);
        assert!(close_kept.is_cancelled());
        assert!(!close_other.is_cancelled());
    }

    struct FakeSubscription {
        payload: bytes::Bytes,
        ack_attempts: Arc<AtomicUsize>,
        fail_ack: bool,
    }

    #[async_trait]
    impl Subscription for FakeSubscription {
        fn subject(&self) -> &str {
            SESSION_CONTROL_SUBJECT
        }

        fn payload(&self) -> &[u8] {
            &self.payload
        }

        async fn ack(&self) -> BusResult<()> {
            self.ack_attempts.fetch_add(1, Ordering::Relaxed);
            if self.fail_ack {
                Err(BusError::Nats("synthetic ACK failure".into()))
            } else {
                Ok(())
            }
        }

        async fn nack(&self) -> BusResult<()> {
            panic!("session-control handler never NACKs")
        }
    }

    #[tokio::test]
    async fn malformed_payload_is_ack_dropped() {
        let ack_attempts = Arc::new(AtomicUsize::new(0));
        let sub = FakeSubscription {
            payload: bytes::Bytes::from_static(b"{not-json"),
            ack_attempts: ack_attempts.clone(),
            fail_ack: false,
        };

        handle_subscription(&Hub::default(), Box::new(sub)).await;

        assert_eq!(ack_attempts.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn ack_transport_failure_is_nonfatal_after_local_apply() {
        let ack_attempts = Arc::new(AtomicUsize::new(0));
        let event = SessionControlEvent::revoke_participant(ParticipantId::new());
        let sub = FakeSubscription {
            payload: serde_json::to_vec(&event).expect("serialize event").into(),
            ack_attempts: ack_attempts.clone(),
            fail_ack: true,
        };

        // The helper logs and returns. The listener remains alive and JetStream
        // may redeliver after ack_wait because no ACK reached the broker.
        handle_subscription(&Hub::default(), Box::new(sub)).await;

        assert_eq!(ack_attempts.load(Ordering::Relaxed), 1);
    }

    #[derive(Debug)]
    struct Published {
        subject: String,
        payload: bytes::Bytes,
        message_id: String,
    }

    #[derive(Default)]
    struct FakeBus {
        plain_publishes: AtomicUsize,
        idempotent_publishes: Mutex<Vec<Published>>,
        subscriptions: Mutex<Vec<(String, Option<String>)>>,
        subscription_ready: tokio::sync::Notify,
    }

    #[async_trait]
    impl EventBus for FakeBus {
        async fn publish(&self, _subject: &str, _payload: bytes::Bytes) -> BusResult<()> {
            self.plain_publishes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn publish_idempotent(
            &self,
            subject: &str,
            payload: bytes::Bytes,
            message_id: &str,
        ) -> BusResult<()> {
            self.idempotent_publishes
                .lock()
                .expect("publish mutex")
                .push(Published {
                    subject: subject.to_owned(),
                    payload,
                    message_id: message_id.to_owned(),
                });
            Ok(())
        }

        async fn subscribe(
            &self,
            subject: &str,
            durable: Option<&str>,
        ) -> BusResult<BoxStream<'static, Box<dyn Subscription + Send>>> {
            self.subscriptions
                .lock()
                .expect("subscription mutex")
                .push((subject.to_owned(), durable.map(str::to_owned)));
            self.subscription_ready.notify_one();
            Ok(Box::pin(futures::stream::pending()))
        }
    }

    #[tokio::test]
    async fn listener_subscribes_ephemerally_and_stops_on_cancellation() {
        let bus = Arc::new(FakeBus::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_with(
            bus.clone(),
            Arc::new(Hub::default()),
            cancel.clone(),
        ));

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            bus.subscription_ready.notified(),
        )
        .await
        .expect("listener subscribed");
        assert_eq!(
            *bus.subscriptions.lock().expect("subscription mutex"),
            vec![(SESSION_CONTROL_SUBJECT.to_owned(), None)]
        );

        cancel.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("listener stopped promptly")
            .expect("listener task joined")
            .expect("listener result");
    }

    #[tokio::test]
    async fn publish_uses_event_id_as_the_idempotency_key() {
        let bus = FakeBus::default();
        let event = SessionControlEvent::revoke_session(ParticipantId::new(), SessionId::new());

        // Reusing the same command for an ambiguous retry must reuse its key.
        publish_to_bus(&bus, &event).await;
        publish_to_bus(&bus, &event).await;

        assert_eq!(bus.plain_publishes.load(Ordering::Relaxed), 0);
        let published = bus.idempotent_publishes.lock().expect("publish mutex");
        assert_eq!(published.len(), 2);
        for call in &*published {
            assert_eq!(call.subject, SESSION_CONTROL_SUBJECT);
            assert_eq!(call.message_id, event.event_id().to_string());
            let decoded: SessionControlEvent =
                serde_json::from_slice(&call.payload).expect("published event");
            assert_eq!(decoded, event);
        }
    }
}
