use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use aero_common::{WebhookDeliveryId, WebhookId};
use aero_storage::{OutgoingTarget, WebhookDelivery};

use super::{rebuild_retry_delivery, settle_bus_delivery, PreparedRetry};

struct SettlementProbe {
    acks: Arc<AtomicUsize>,
    nacks: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl aero_bus::Subscription for SettlementProbe {
    fn subject(&self) -> &'static str {
        "im.room.test"
    }

    fn payload(&self) -> &[u8] {
        b"{}"
    }

    async fn ack(&self) -> aero_bus::traits::BusResult<()> {
        self.acks.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    async fn nack(&self) -> aero_bus::traits::BusResult<()> {
        self.nacks.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn complete_fanout_acks_and_incomplete_fanout_nacks() {
    let probe = SettlementProbe {
        acks: Arc::new(AtomicUsize::new(0)),
        nacks: Arc::new(AtomicUsize::new(0)),
    };
    let acks = probe.acks.clone();
    let nacks = probe.nacks.clone();
    settle_bus_delivery(Box::new(probe), true).await.unwrap();
    settle_bus_delivery(
        Box::new(SettlementProbe {
            acks: acks.clone(),
            nacks: nacks.clone(),
        }),
        false,
    )
    .await
    .unwrap();
    assert_eq!(acks.load(Ordering::Acquire), 1);
    assert_eq!(nacks.load(Ordering::Acquire), 1);
}

#[test]
fn retry_reuses_immutable_body_and_refreshes_signature() {
    let body = br#"{"kind":"message","body":"original"}"#.to_vec();
    let prepared = PreparedRetry {
        delivery: WebhookDelivery {
            id: WebhookDeliveryId::new(),
            webhook_id: WebhookId::new(),
            event_id: Some("event-1".to_owned()),
            request_body: Some(body.clone()),
            request_headers: vec![
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("traceparent".to_owned(), "00-trace-parent-01".to_owned()),
            ],
            claim_token: uuid::Uuid::new_v4(),
            status: "pending".to_owned(),
            attempts: 2,
            last_status_code: None,
            last_error: None,
            next_attempt_at: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        },
        target: OutgoingTarget {
            id: WebhookId::new(),
            url: "https://hooks.example/events".to_owned(),
            secret: "current-secret".to_owned(),
            breaker: aero_storage::BreakerState::default(),
        },
    };

    let retry = rebuild_retry_delivery(&prepared, 99).expect("body is retained");
    let expected_signature = aero_storage::sign_payload("current-secret", 99, &body);
    assert_eq!(retry.body, body);
    assert_eq!(retry.header(aero_storage::TIMESTAMP_HEADER), Some("99"));
    assert_eq!(
        retry.header(aero_storage::SIGNATURE_HEADER),
        Some(expected_signature.as_str())
    );
    assert_eq!(retry.header("traceparent"), Some("00-trace-parent-01"));
    assert!(
        !String::from_utf8_lossy(&retry.body).contains("\"retry\""),
        "retry must not replace the original event with a marker payload"
    );
}
